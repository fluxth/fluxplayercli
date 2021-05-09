use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{
        AtomicUsize, AtomicBool,
        Ordering::{Relaxed, SeqCst}
    }
};

use portaudio as pa;
use ringbuf::Producer;
use ffmpeg::{
    frame::Audio, 
    time::sleep,
    format::{
        Sample,
        sample::Type::Packed
    }
};

const CHANNELS: i32 = 2;
const SAMPLE_RATE: f64 = 48000.0;
const FRAMES_PER_BUFFER: u32 = 512;
const BUFFER_SIZE: usize = SAMPLE_RATE as usize * CHANNELS as usize;

const SAMPLE_TYPE: Sample = Sample::F32(Packed);
const CHANNEL_LAYOUT: ffmpeg::ChannelLayout = ffmpeg::ChannelLayout::STEREO;

const GAIN: f32 = 0.5;

struct PlayerStatus {
    is_decoding: AtomicBool,
    is_playing: AtomicBool,
    frames_decoded: AtomicUsize,
    frames_played: AtomicUsize,
}

impl PlayerStatus {
    fn new() -> Self {
        Self {
            is_decoding: AtomicBool::new(false),
            is_playing: AtomicBool::new(false),
            frames_decoded: AtomicUsize::new(0),
            frames_played: AtomicUsize::new(0),
        }
    }
}

const METADATA_WHITELIST: [&str; 7] = [
    "title", "artist", "album", "album_artist", "track", "disc", "genre"
];

fn main() {
    println!("fluxplayer cli\n");
    let path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            println!("usage: ./fluxplayercli <in_file>");
            return;   
        }
    };

    ffmpeg::init().unwrap();
    if let Ok(ref mut input) = ffmpeg::format::input(&path) {
        println!("{}[Input]", " ".repeat(17)); 
        println!("{:>16}: {}", 
                "File Path", &path);
        println!("{:>16}: {} ({})", 
                "Container", input.format().name(), input.format().description());

        for (key, val) in input.metadata().iter() {
            if METADATA_WHITELIST.contains(&key) {
                println!("{:>16}: {}", key, val);
            }
        }

        if let Some(ref stream) = input.streams().best(ffmpeg::media::Type::Audio) {
            let stream_index = stream.index();
            let start_pts = stream.start_time();
            let duration_pts = stream.duration();
            let duration_sec = duration_pts as f64 * f64::from(stream.time_base());

            let codec = stream.codec();

            println!("\n{}[Stream {}]", " ".repeat(17), stream.index());
            println!("{:>16}: {:?} - {:?}", 
                    "Type", codec.medium(), codec.id());
            println!("{:>16}: {}", 
                    "Time Base", stream.time_base());
            println!("{:>16}: {} / {}", 
                    "Start / Dur.", start_pts, duration_pts);
            println!("{:>16}: {}", 
                    "Decode Frames", stream.frames());

            if let Ok(ref mut audio) = codec.decoder().audio() {
                let file_sample_rate = audio.rate();

                println!("{:>16}: {:.1} kbps (Max: {:.1} kbps)", 
                    "Bit Rate", 
                    audio.bit_rate() as f64 / 1000.,
                    audio.max_bit_rate() as f64 / 1000.
                );
                println!("{:>16}: {:?}", 
                        "Format", audio.format());
                println!("{:>16}: {}", 
                        "Sample Rate", file_sample_rate);
                println!("{:>16}: {:?}", 
                        "Channel Layout", audio.channel_layout());

                let resample = !(audio.format() == SAMPLE_TYPE
                    && (audio.channel_layout() & CHANNEL_LAYOUT) == CHANNEL_LAYOUT
                    && audio.rate() as f64 == SAMPLE_RATE);

                println!("\n{}[Resampler]", " ".repeat(17));
                println!("{:>16}: {}", 
                        "Enabled", resample);

                if resample {
                    println!("{:>16}: {:?} -> {:?}", 
                            "Format", audio.format(), SAMPLE_TYPE);
                    println!("{:>16}: {} -> {}", 
                            "Sample Rate", file_sample_rate as f64, SAMPLE_RATE);
                    println!("{:>16}: {} -> 2", 
                            "Channels", audio.channels());                       
                }

                let mut swr: Option<ffmpeg::software::resampling::Context> = None;
                if resample {
                    swr = Some(
                        ffmpeg::software::resampler(
                            (audio.format(), audio.channel_layout(), file_sample_rate),
                            (SAMPLE_TYPE, CHANNEL_LAYOUT, SAMPLE_RATE as u32),
                        )
                        .unwrap(),
                    );
                }

                let pa = pa::PortAudio::new().unwrap();
                let pa_settings = pa
                    .default_output_stream_settings::<f32>(CHANNELS, SAMPLE_RATE, FRAMES_PER_BUFFER)
                    .expect("Could not set output stream settings.");

                println!("\n{}[Play Device]", " ".repeat(17));
                let default_out = pa.device_info(pa.default_output_device().unwrap()).unwrap();
                println!("{:>16}: {}", 
                        "Driver", pa.host_api_info(default_out.host_api).unwrap().name);
                println!("{:>16}: {}", 
                        "Output Device", default_out.name);

                let ringbuffer = ringbuf::RingBuffer::<f32>::new(BUFFER_SIZE);
                let (mut rb_tx, mut rb_rx) = ringbuffer.split();

                let mut status = Arc::new(PlayerStatus::new());

                let status_cb = status.clone();
                let status_o = status.clone();

                let callback = move |pa::OutputStreamCallbackArgs { buffer, frames, .. }| {
                    let recv_size = rb_rx.pop_slice(buffer);
                    assert_eq!(recv_size % CHANNELS as usize, 0);

                    let mut idx = 0;
                    for _ in 0..frames {
                        for _ in 0..CHANNELS {
                            if idx >= recv_size {
                                buffer[idx] = 0f32;
                            } else {
                                buffer[idx] *= GAIN;
                            }
                            idx += 1;
                        }

                        status_cb.frames_played.fetch_add(1, SeqCst);
                    }

                    if !status_cb.is_decoding.load(SeqCst) && rb_rx.is_empty() && recv_size == 0 {
                        status_cb.is_playing.store(false, SeqCst);
                        return pa::Complete;
                    }

                    pa::Continue
                };

                let mut pa_stream = pa.open_non_blocking_stream(pa_settings, callback)
                    .expect("Could not open output device.");

                let mut decode_frame = ffmpeg::frame::Audio::empty();
                let mut swr_frame = ffmpeg::frame::Audio::empty();

                if pa_stream.start().is_ok() {
                    status.is_playing.store(true, SeqCst);
                } else {
                    panic!("Play failed!");
                }

                let othread_handle = std::thread::spawn(move || {
                    println!(
                        "\n  DECODE  PLAYPOS DURATION"
                    );
                    while status_o.is_playing.load(Relaxed) {
                        print!(
                            "\r{:>7.1}s {:>7.1}s {:>7.1}s  [PLAYING]",
                            status_o.frames_decoded.load(Relaxed) as f64 / SAMPLE_RATE,
                            status_o.frames_played.load(Relaxed) as f64 / SAMPLE_RATE,
                            duration_sec
                        );
                        let _ = io::stdout().flush();

                        sleep(100_000).unwrap();
                    }
                    print!("\n");
                });

                let mut packets = input.packets();
                while let Some(Ok((read_stream, read_packet))) = packets.next() {
                    if read_stream.index() == stream_index {
                        match audio.decode(&read_packet, &mut decode_frame) {
                            Ok(true) => {
                                let ts = decode_frame.timestamp();
                                decode_frame.set_pts(ts);

                                if resample {
                                    if swr.as_mut().unwrap().run(&decode_frame, &mut swr_frame).is_ok() {
                                        send_audio(&mut swr_frame, &mut rb_tx, &mut status);
                                        let _ = status.is_decoding
                                            .compare_exchange_weak(false, true, SeqCst, Relaxed);
                                    }
                                } else {
                                    send_audio(&mut decode_frame, &mut rb_tx, &mut status);
                                    let _ = status.is_decoding
                                        .compare_exchange_weak(false, true, SeqCst, Relaxed);
                                }
                            }
                            Ok(_) => (),
                            Err(e) => eprintln!("Error: {:?}", e),
                        }
                    }
                }

                if resample && swr.as_ref().unwrap().delay().is_some() {
                    while let Ok(Some(_)) = swr.as_mut().unwrap().flush(&mut swr_frame) {
                        send_audio(&mut swr_frame, &mut rb_tx, &mut status);
                        let _ = status.is_decoding.compare_exchange_weak(false, true, SeqCst, Relaxed);
                    }
                }

                status.is_decoding.store(false, Relaxed);
                while status.is_playing.load(Relaxed) {
                    sleep(1_000_000).unwrap();
                }

                othread_handle.join().unwrap();

                pa_stream.stop().unwrap();
                pa_stream.close().unwrap();
            }
        }
    }
}

#[inline]
fn send_audio(audio_frame: &mut Audio, rb_tx: &mut Producer<f32>, status: &mut Arc<PlayerStatus>) {
    // void* arrays in C makes me unsafe :(
    let (head, data, tail) = unsafe { audio_frame.data(0).align_to::<f32>() };

    assert!(head.is_empty() && tail.is_empty());

    let mut sent_size = 0;
    while sent_size < data.len() {
        if sent_size > 0 {
            sleep(10_000).unwrap();
        }

        let current_size = rb_tx.push_slice(&data[sent_size..]);
        sent_size += current_size;

        assert_eq!(sent_size % CHANNELS as usize, 0);

        status.frames_decoded.fetch_add(current_size / CHANNELS as usize, Relaxed);
    }
}
