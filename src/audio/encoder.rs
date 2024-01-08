extern crate ffmpeg_next as ffmpeg;
use anyhow::{Context, Result};
use ffmpeg::{codec, filter, format, frame, media};
use ffmpeg::{rescale, Rescale};
use log::debug;
use std::path::{Path, PathBuf};

fn filter(spec: &str, decoder: &codec::decoder::Audio, encoder: &codec::encoder::Audio) -> Result<filter::Graph> {
    let mut filter = filter::Graph::new();

    let channel_layout = if !decoder.channel_layout().is_empty() {
        decoder.channel_layout()
    } else {
        ffmpeg_next::channel_layout::ChannelLayout::MONO
    };

    let args = format!(
        "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        decoder.time_base(),
        decoder.rate(),
        decoder.format().name(),
        channel_layout
    );
    debug!("args are {args}");

    filter.add(&filter::find("abuffer").context("cant find abuffer")?, "in", &args)?;
    filter.add(&filter::find("abuffersink").context("cant find abuffersink")?, "out", "")?;

    {
        let mut out = filter.get("out").context("out is none")?;

        out.set_sample_format(encoder.format());
        out.set_channel_layout(ffmpeg_next::channel_layout::ChannelLayout::MONO); // TODO change
        out.set_sample_rate(encoder.rate());
    }

    filter.output("in", 0)?.input("out", 0)?.parse(spec)?;
    filter.validate()?;

    println!("{}", filter.dump());

    if let Some(codec) = encoder.codec() {
        if !codec
            .capabilities()
            .contains(ffmpeg::codec::capabilities::Capabilities::VARIABLE_FRAME_SIZE)
        {
            filter
                .get("out")
                .context("out is none")?
                .sink()
                .set_frame_size(encoder.frame_size());
        }
    }

    Ok(filter)
}

struct Transcoder {
    stream: usize,
    filter: filter::Graph,
    decoder: codec::decoder::Audio,
    encoder: codec::encoder::Audio,
    in_time_base: ffmpeg::Rational,
    out_time_base: ffmpeg::Rational,
}

fn transcoder<P: AsRef<Path>>(
    ictx: &mut format::context::Input,
    octx: &mut format::context::Output,
    path: &P,
    filter_spec: &str,
) -> Result<Transcoder> {
    let input = ictx
        .streams()
        .best(media::Type::Audio)
        .expect("could not find best audio stream");
    let context = ffmpeg::codec::context::Context::from_parameters(input.parameters())?;
    let mut decoder = context.decoder().audio()?;
    let codec = ffmpeg::encoder::find(octx.format().codec(path, media::Type::Audio))
        .expect("failed to find encoder")
        .audio()?;
    let global = octx.format().flags().contains(ffmpeg::format::flag::Flags::GLOBAL_HEADER);

    decoder.set_parameters(input.parameters())?;

    let mut output = octx.add_stream(codec)?;
    let context = ffmpeg::codec::context::Context::from_parameters(output.parameters())?;
    let mut encoder = context.encoder().audio()?;

    let channel_layout = codec
        .channel_layouts()
        .map(|cls| cls.best(decoder.channel_layout().channels()))
        .unwrap_or(ffmpeg::channel_layout::ChannelLayout::STEREO);

    if global {
        encoder.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    encoder.set_rate(decoder.rate() as i32);
    encoder.set_channel_layout(channel_layout);
    encoder.set_channels(channel_layout.channels());
    encoder.set_format(
        codec
            .formats()
            .context("unknown supported formats")?
            .next()
            .context("codec not found")?,
    );
    encoder.set_bit_rate(decoder.bit_rate());
    encoder.set_max_bit_rate(decoder.max_bit_rate());

    encoder.set_time_base((1, decoder.rate() as i32));
    output.set_time_base((1, decoder.rate() as i32));

    let encoder = encoder.open_as(codec)?;
    output.set_parameters(&encoder);

    let filter = filter(filter_spec, &decoder, &encoder)?;

    let in_time_base = decoder.time_base();
    let out_time_base = output.time_base();

    Ok(Transcoder {
        stream: input.index(),
        filter,
        decoder,
        encoder,
        in_time_base,
        out_time_base,
    })
}

impl Transcoder {
    fn send_frame_to_encoder(&mut self, frame: &ffmpeg::Frame) -> Result<()> {
        self.encoder.send_frame(frame)?;
        Ok(())
    }

    fn send_eof_to_encoder(&mut self) -> Result<()> {
        self.encoder.send_eof()?;
        Ok(())
    }

    fn receive_and_process_encoded_packets(&mut self, octx: &mut format::context::Output) -> Result<()> {
        let mut encoded = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(0);
            encoded.rescale_ts(self.in_time_base, self.out_time_base);
            encoded.write_interleaved(octx)?;
        }
        Ok(())
    }

    fn add_frame_to_filter(&mut self, frame: &ffmpeg::Frame) -> Result<()> {
        self.filter.get("in").context("in is none")?.source().add(frame)?;
        Ok(())
    }

    fn flush_filter(&mut self) -> Result<()> {
        self.filter.get("in").context("in is none")?.source().flush()?;
        Ok(())
    }

    fn get_and_process_filtered_frames(&mut self, octx: &mut format::context::Output) -> Result<()> {
        let mut filtered = frame::Audio::empty();
        while self
            .filter
            .get("out")
            .context("out is none")?
            .sink()
            .frame(&mut filtered)
            .is_ok()
        {
            self.send_frame_to_encoder(&filtered);
            self.receive_and_process_encoded_packets(octx);
        }
        Ok(())
    }

    fn send_packet_to_decoder(&mut self, packet: &ffmpeg::Packet) -> Result<()> {
        self.decoder.send_packet(packet)?;
        Ok(())
    }

    fn send_eof_to_decoder(&mut self) -> Result<()> {
        self.decoder.send_eof()?;
        Ok(())
    }

    fn receive_and_process_decoded_frames(&mut self, octx: &mut format::context::Output) {
        let mut decoded = frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let timestamp = decoded.timestamp();
            decoded.set_pts(timestamp);
            self.add_frame_to_filter(&decoded);
            self.get_and_process_filtered_frames(octx);
        }
    }
}

// Transcode the `best` audio stream of the input file into a the output file while applying a
// given filter. If no filter was specified the stream gets copied (`anull` filter).
//
// Example 1: Transcode *.mp3 file to *.wmv while speeding it up
// transcode-audio in.mp3 out.wmv "atempo=1.2"
//
// Example 2: Overlay an audio file
// transcode-audio in.mp3 out.mp3 "amovie=overlay.mp3 [ov]; [in][ov] amerge [out]"
//
// Example 3: Seek to a specified position (in seconds)
// transcode-audio in.mp3 out.mp3 anull 30
pub fn convert_to_16khz(input: PathBuf, output: PathBuf, filter: Option<String>, seek: Option<String>) -> Result<()> {
    ffmpeg::init()?;

    let filter = filter.unwrap_or_else(|| "anull".to_owned());
    let seek = seek.and_then(|s| s.parse::<i64>().ok());

    let mut ictx = format::input(&input)?;
    let mut octx = format::output(&output)?;
    let mut transcoder = transcoder(&mut ictx, &mut octx, &output, &filter)?;

    if let Some(position) = seek {
        // If the position was given in seconds, rescale it to ffmpegs base timebase.
        let position = position.rescale((1, 1), rescale::TIME_BASE);
        // If this seek was embedded in the transcoding loop, a call of `flush()`
        // for every opened buffer after the successful seek would be advisable.
        ictx.seek(position, ..position)?;
    }
    octx.set_metadata(ictx.metadata().to_owned());
    octx.write_header()?;

    for (stream, mut packet) in ictx.packets() {
        if stream.index() == transcoder.stream {
            packet.rescale_ts(stream.time_base(), transcoder.in_time_base);
            transcoder.send_packet_to_decoder(&packet);
            transcoder.receive_and_process_decoded_frames(&mut octx);
        }
    }

    transcoder.send_eof_to_decoder();
    transcoder.receive_and_process_decoded_frames(&mut octx);

    transcoder.flush_filter();
    transcoder.get_and_process_filtered_frames(&mut octx);

    transcoder.send_eof_to_encoder();
    transcoder.receive_and_process_encoded_packets(&mut octx);

    octx.write_trailer()?;
    Ok(())
}
