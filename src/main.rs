use crate::converter::ToAsciiArt;
use clap::Parser;
use ffmpeg_next as ffmpeg;
use image::{ImageBuffer, Luma};
use rodio::{self};

use std::{
    io::{self, stdout, BufReader, Stdout},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossterm::{
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
    #[arg(long, default_value = "160")]
    width: u32,
    /// The height of the ASCII art
    #[arg(long, default_value = "90")]
    height: u32,
    /// The gamma of the ASCII art
    #[arg(long, default_value = "1.0")]
    gamma: f32,
    /// The target frame rate
    #[arg(long, default_value = "60.0")]
    frame_rate: Option<f32>,
    /// Whether or not to live edit the ASCII art
    #[arg(long, default_value = "false")]
    live: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let result = App::run_video(args.file.clone(), args); // Call video run method
    println!("{}", result.unwrap());
    Ok(())
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
            width: 160,
            height: 90,
            gamma: 1.0,
            selected_field: Fields::Width,
        }
    }

    pub fn run_video(file: String, args: Args) -> io::Result<String> {
        // Initialize ffmpeg and open the video file
        ffmpeg::init().unwrap();

        let running = Arc::new(Mutex::new(true));

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
            let result = Self::play_audio(&file);
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

        // Get the video's time base
        let video_time_base: f64 = video_stream.time_base().into();
        let start_time = Instant::now();

        // Process each packet in the video
        for (stream, packet) in ictx.packets() {
            if !*running.lock().unwrap() {
                break;
            }

            if stream.index() == video_stream_index {
                video_decoder.send_packet(&packet)?;

                let mut decoded = ffmpeg::frame::Video::empty();

                while video_decoder.receive_frame(&mut decoded).is_ok() {
                    if let Some(pts) = decoded.pts() {
                        let frame_timestamp = pts as f64 * video_time_base;

                        let elapsed = Instant::now().duration_since(start_time);
                        let elapsed_secs = elapsed.as_secs_f64();

                        if frame_timestamp > elapsed_secs {
                            let sleep_duration =
                                Duration::from_secs_f64(frame_timestamp - elapsed_secs);
                            std::thread::sleep(sleep_duration);
                        }
                    }

                    // Convert the frame to RGB
                    let mut rgb_frame = ffmpeg::frame::Video::empty();
                    scaler.run(&decoded, &mut rgb_frame)?;

                    // Convert the frame to a grayscale ImageBuffer
                    let image: ImageBuffer<Luma<u8>, Vec<u8>> = ImageBuffer::from_raw(
                        rgb_frame.width(),
                        rgb_frame.height(),
                        rgb_frame.data(0).iter().step_by(3).copied().collect(),
                    )
                    .unwrap();

                    // Convert the image to ASCII art
                    let options = converter::AsciiOptions::new(args.width, args.width, args.gamma); // Adjust options as needed
                    app.art = converter::ImageConverter::from_image_buffer(image)
                        .to_ascii_art(Some(options));

                    // Draw the updated ASCII art in the terminal
                    let _ = terminal.draw(|frame| app.ui(frame));
                }
            }
        }

        Ok(())
    }

    fn play_audio(file: &str) -> Result<(), Box<dyn std::error::Error>> {
        let music_file = std::fs::File::open(file).unwrap();
        let decoder = rodio::Decoder::new(BufReader::new(music_file)).unwrap();
        let (_stream, stream_handle) = rodio::OutputStream::try_default()?;
        let sink = rodio::Sink::try_new(&stream_handle)?;

        sink.append(decoder);

        sink.sleep_until_end();
        Ok(())
    }

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
