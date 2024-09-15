use crate::converter::ToAsciiArt;
use clap::Parser;
use ffmpeg_next as ffmpeg;
use image::{io::Reader as ImageReader, ImageBuffer, Rgb};
use rodio::{self, Decoder, Source};

use std::{
    io::{self, stdout, BufReader, Stdout},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};

use ratatui::{
    prelude::*,
    widgets::{canvas::*, *},
};

mod converter;

#[derive(Parser, Clone, Debug)]
#[command(author,version,about,long_about = None)]
pub struct Args {
    /// The path to an image file
    #[arg(long, default_value = "")]
    file: String,
    /// The width of the ASCII art
    #[arg(long, default_value = "240")]
    width: u32,
    /// The height of the ASCII art
    #[arg(long, default_value = "120")]
    height: u32,
    /// The gamma of the ASCII art
    #[arg(long, default_value = "0.8")]
    gamma: f32,
    /// The target frame rate
    #[arg(long, default_value = "30.0")]
    frame_rate: Option<f32>,
    /// Whether or not to live edit the ASCII art
    #[arg(long, default_value = "false")]
    live: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match args.file.ends_with(".mp4") {
        true => {
            let result = App::run_video(args.file.clone(), args); // Call video run method
            println!("{}", result.unwrap());
            Ok(())
        }
        false => match args.live {
            true => {
                let file = args.file;

                if !std::path::Path::new(&file).exists() {
                    return Err("File does not exist".into());
                }

                let result = App::run(file);

                println!("{}", result.unwrap());

                Ok(())
            }
            false => {
                let open_file = ImageReader::open(args.file).unwrap();
                let image = open_file.decode().unwrap();
                let converter = converter::ImageConverter::new(image);
                let options = converter::AsciiOptions::new(args.width, args.height, args.gamma);
                let art = converter.to_ascii_art(Some(options));

                println!("{}", art);

                Ok(())
            }
        },
    }
}

struct App {
    art: String,
    width: u32,
    height: u32,
    gamma: f32,
    selected_field: Fields,
}

#[derive(PartialEq)]
enum Fields {
    Width,
    Height,
    Gamma,
    Finish,
}

impl App {
    fn new() -> App {
        App {
            art: String::new(),
            width: 80,
            height: 50,
            gamma: 1.0,
            selected_field: Fields::Width,
        }
    }

    pub fn run_video(file: String, args: Args) -> io::Result<String> {
        // Initialize ffmpeg and open the video file
        ffmpeg::init().unwrap();

        let running = Arc::new(Mutex::new(true));
        let running_clone = Arc::clone(&running);

        let mut terminal = init_terminal()?;
        let mut app = App::new();

        // Video playback thread
        let video_file = file.clone();
        let video_args = args.clone();
        let video_thread = std::thread::spawn(move || {
            let result =
                Self::play_video(&video_file, &video_args, &mut terminal, &mut app, &running);
            if let Err(e) = result {
                eprintln!("Video playback error: {}", e);
            }
        });
        // Audio playback thread
        let audio_thread = std::thread::spawn(move || {
            let result = Self::play_audio(&file, &running_clone);
            if let Err(e) = result {
                eprintln!("Audio playback error: {}", e);
            }
        });

        // Wait for both threads to finish
        video_thread.join().unwrap();
        audio_thread.join().unwrap();

        let _ = restore_terminal();

        Ok("Video and audio playback finished".to_string())
    }

    fn play_video(
        file: &str,
        args: &Args,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        app: &mut App,
        running: &Arc<Mutex<bool>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut ictx = ffmpeg::format::input(&file)?;

        // Find the best video stream
        let video_stream = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;
        let video_stream_index = video_stream.index();
        let video_context_decoder =
            ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?;
        let mut video_decoder = video_context_decoder.decoder().video()?;

        // Create a scaler to convert the video frames to RGB format
        let mut scaler = ffmpeg::software::scaling::context::Context::get(
            video_decoder.format(),
            video_decoder.width(),
            video_decoder.height(),
            ffmpeg::format::Pixel::RGB24,
            video_decoder.width(),
            video_decoder.height(),
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )?;

        let video_frame_rate = f64::from(video_stream.rate());
        let target_frame_rate = args.frame_rate.unwrap_or(video_frame_rate as f32);
        let frame_time_ns = (1e9 / target_frame_rate as f64) as u64; // Calculate frame duration in nanoseconds

        // Process each packet in the video
        for (stream, packet) in ictx.packets() {
            if !*running.lock().unwrap() {
                break;
            }

            if stream.index() == video_stream_index {
                video_decoder.send_packet(&packet)?;

                let mut decoded = ffmpeg::frame::Video::empty();

                while video_decoder.receive_frame(&mut decoded).is_ok() {
                    let mut rgb_frame = ffmpeg::frame::Video::empty();
                    scaler.run(&decoded, &mut rgb_frame)?;

                    // Convert the frame to an image::ImageBuffer
                    let image: ImageBuffer<Rgb<u8>, _> = ImageBuffer::from_raw(
                        rgb_frame.width(),
                        rgb_frame.height(),
                        rgb_frame.data(0).to_vec(),
                    )
                    .unwrap();

                    // Convert the image to ASCII art
                    let options = converter::AsciiOptions::new(800, 400, 1.0); // Set options
                    app.art = converter::ImageConverter::from_image_buffer(image)
                        .to_ascii_art(Some(options));

                    // Draw the updated ASCII art in the terminal
                    let _ = terminal.draw(|frame| app.ui(frame));

                    // Optional: Add a delay for frame rate control
                    let start_time = Instant::now();
                    let processed_frames = 1; // Assuming one frame processed
                    let target_time =
                        start_time + Duration::from_nanos(processed_frames * frame_time_ns);
                    let now = Instant::now();
                    if now < target_time {
                        std::thread::sleep(target_time - now);
                    }
                }
            }
        }

        Ok(())
    }

    fn play_audio(
        file: &str,
        running: &Arc<Mutex<bool>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut ictx = ffmpeg::format::input(&file)?;
        let music_file = std::fs::File::open(file).unwrap();
        let decoder = rodio::Decoder::new(BufReader::new(music_file)).unwrap();
        let (_stream, stream_handle) = rodio::OutputStream::try_default()?;
        let sink = rodio::Sink::try_new(&stream_handle)?;

        sink.append(decoder);
        // let audio_stream = ictx
        //     .streams()
        //     .best(ffmpeg::media::Type::Audio)
        //     .ok_or(ffmpeg::Error::StreamNotFound)?;
        // let audio_stream_index = audio_stream.index();
        // let audio_context_decoder =
        //     ffmpeg::codec::context::Context::from_parameters(audio_stream.parameters())?;
        // let mut audio_decoder = audio_context_decoder.decoder().audio()?;

        // let sample_rate = u32::from(audio_stream.rate());

        // for (stream, packet) in ictx.packets() {
        //     if stream.index() == audio_stream_index {
        //         audio_decoder.send_packet(&packet)?;
        //         let mut audio_frame = ffmpeg::frame::Audio::empty();

        //         while audio_decoder.receive_frame(&mut audio_frame).is_ok() {
        //             let samples: Vec<i16> = audio_frame.data(0).iter().map(|&s| s as i16).collect();

        //             let source = rodio::buffer::SamplesBuffer::new(
        //                 audio_frame.channels(),
        //                 sample_rate,
        //                 samples,
        //             );

        //             sink.append(source);
        //         }
        //     }
        // }

        sink.sleep_until_end();
        Ok(())
    }

    pub fn run(file: String) -> io::Result<String> {
        let mut terminal = init_terminal()?;
        let mut app = App::new();
        let mut last_tick = Instant::now();
        let tick_rate = Duration::from_millis(33);

        let open_file = ImageReader::open(file).unwrap();
        let image = open_file.decode().unwrap();
        let converter = converter::ImageConverter::new(image);

        loop {
            let options = converter::AsciiOptions::new(app.width, app.height, app.gamma);
            app.art = converter.to_ascii_art(Some(options));

            let _ = terminal.draw(|frame| app.ui(frame));
            let timeout = tick_rate.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Right => match app.selected_field {
                                Fields::Width => {
                                    app.width += 1;
                                }
                                Fields::Height => {
                                    app.height += 1;
                                }
                                Fields::Gamma => {
                                    app.gamma += 0.1;
                                }
                                Fields::Finish => {}
                            },
                            KeyCode::Left => match app.selected_field {
                                Fields::Width => {
                                    app.width -= 1;
                                }
                                Fields::Height => {
                                    app.height -= 1;
                                }
                                Fields::Gamma => {
                                    app.gamma -= 0.1;
                                }
                                Fields::Finish => {}
                            },
                            KeyCode::Up => match app.selected_field {
                                Fields::Width => {
                                    app.selected_field = Fields::Finish;
                                }
                                Fields::Height => {
                                    app.selected_field = Fields::Width;
                                }
                                Fields::Gamma => {
                                    app.selected_field = Fields::Height;
                                }
                                Fields::Finish => {
                                    app.selected_field = Fields::Gamma;
                                }
                            },
                            KeyCode::Down => match app.selected_field {
                                Fields::Width => {
                                    app.selected_field = Fields::Height;
                                }
                                Fields::Height => {
                                    app.selected_field = Fields::Gamma;
                                }
                                Fields::Gamma => {
                                    app.selected_field = Fields::Finish;
                                }
                                Fields::Finish => {
                                    app.selected_field = Fields::Width;
                                }
                            },
                            KeyCode::Enter => match app.selected_field {
                                Fields::Finish => {
                                    break;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }

            if last_tick.elapsed() >= tick_rate {
                app.on_tick();
                last_tick = Instant::now();
            }
        }

        let _ = restore_terminal();

        return Ok(app.art);
    }

    fn on_tick(&mut self) {}

    fn ui(&self, frame: &mut Frame) {
        let main_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(80), Constraint::Percentage(20)].as_ref())
            .split(frame.size());

        frame.render_widget(self.boxes_canvas(main_layout[0]), main_layout[0]);
        frame.render_widget(self.boxes_options(main_layout[1]), main_layout[1]);
    }

    fn boxes_options(&self, area: Rect) -> impl Widget {
        let (left, right, bottom, top) =
            (0.0, area.width as f64, 0.0, area.height as f64 * 2.0 - 4.0);

        let width = self.width.to_string();
        let width_text = format!(
            "Width: {} {}",
            width,
            if self.selected_field == Fields::Width {
                "<"
            } else {
                ""
            }
        );

        let height = self.height.to_string();
        let height_text = format!(
            "Height: {} {}",
            height,
            if self.selected_field == Fields::Height {
                "<"
            } else {
                ""
            }
        );

        let gamma = self.gamma.to_string();
        let gamma_text = format!(
            "Gamma: {} {}",
            gamma,
            if self.selected_field == Fields::Gamma {
                "<"
            } else {
                ""
            }
        );

        let confirm_text = format!(
            "Confirm {}",
            if self.selected_field == Fields::Finish {
                "<"
            } else {
                ""
            }
        );

        Canvas::default()
            .block(Block::default().borders(Borders::ALL).title("Options"))
            .x_bounds([left, right])
            .y_bounds([bottom, top])
            .paint(move |ctx| {
                ctx.draw(&Rectangle {
                    x: left,
                    y: bottom,
                    width: right - left,
                    height: top - bottom,
                    color: Color::White,
                });

                ctx.print(2.0, top - 4.0, width_text.clone());
                ctx.print(2.0, top - 6.0, height_text.clone());
                ctx.print(2.0, top - 8.0, gamma_text.clone());
                ctx.print(2.0, bottom + 1.0, confirm_text.clone());
            })
    }

    fn boxes_canvas(&self, area: Rect) -> impl Widget {
        let (left, right, bottom, top) =
            (0.0, area.width as f64, 0.0, area.height as f64 * 2.0 - 4.0);

        let art = self.art.clone();

        Canvas::default()
            .block(Block::default().borders(Borders::ALL).title("Art"))
            .x_bounds([left, right])
            .y_bounds([bottom, top])
            .paint(move |ctx| {
                ctx.draw(&Rectangle {
                    x: left,
                    y: bottom,
                    width: right - left,
                    height: top - bottom,
                    color: Color::White,
                });
                let mut x = 1.0;
                let mut y = top - 2.0;

                for c in art.chars() {
                    if c == '\n' {
                        x = 1.0;
                        y -= 1.0;
                        continue;
                    }
                    ctx.print(x, y, c.to_string());
                    x += 1.0;
                }
            })
    }
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout()))
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
