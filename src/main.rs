use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use reqwest::blocking::{multipart, Client};
use which::which;

slint::slint! {
    import { Button, VerticalBox, HorizontalBox, TextEdit } from "std-widgets.slint";
    export component MainWindow inherits Window {
        min-width: 640px;
        min-height: 480px;
        callback record_pressed <=> record.clicked;
        callback refine_pressed <=> refine.clicked;
        in-out property <string> status_text: "Idle";
        in-out property <string> transcript_text: "";
        in-out property <bool> show_refine_button: true;
        VerticalBox {
            HorizontalBox {
                status := Text {
                    text: status_text;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
                transcript := TextEdit {
                    text: transcript_text;
                    read-only: true;
                }
            }
            HorizontalBox {
                record := Button { text: "Record"; }
                refine := Button { text: "Refine"; visible: show_refine_button; }
            }
        }
    }
}

/// Represents the current state of the application.
#[derive(Eq, PartialEq)]
enum State {
    Stopped,
    Recording,
    Processing,
}

/// Handles state transitions triggered by the record button.
fn handle_window_state_change(window: slint::Weak<MainWindow>, state: &mut State, api_key: &str) {
    let upgraded = window.upgrade().expect("Window upgrade failed");
    match *state {
        State::Stopped => {
            *state = State::Recording;
            upgraded.set_status_text("Recording...".into());
            // Specifically not waiting on this. Areceord cant block the main loop
            let _ = Command::new("arecord")
                .args(&["-f", "cd", "-t", "wav", "-q", "/tmp/whisper_record.wav"])
                .spawn()
                .expect("Failed to start recording");
        }
        State::Recording => {
            let _ = Command::new("pkill")
                .arg("arecord")
                .spawn()
                .expect("Failed to stop recording")
                .wait();
            *state = State::Processing;
            upgraded.set_status_text("Processing...".into());

            let file_path = "/tmp/whisper_record.wav";
            if !std::path::Path::new(file_path).exists() {
                eprintln!("Error: Recorded file does not exist!");
                upgraded.set_transcript_text("Error: Recording failed".into());
                upgraded.set_status_text("Error".into());
                *state = State::Stopped;
                return;
            }
            let file_size = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
            if file_size == 0 {
                eprintln!("Error: Recorded file is empty!");
                upgraded.set_transcript_text("Error: Empty recording".into());
                upgraded.set_status_text("Error".into());
                *state = State::Stopped;
                return;
            }
            if file_size > 25 * 1024 * 1024 {
                eprintln!(
                    "Error: Audio file is too large ({} bytes). Maximum is 25 MB.",
                    file_size
                );
                upgraded.set_transcript_text("Error: Audio file too large".into());
                upgraded.set_status_text("Error".into());
                *state = State::Stopped;
                return;
            }
            println!("File size: {} bytes", file_size);

            let client = Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("Failed to build HTTP client");

            let mut attempts = 3;
            let mut transcript = String::new();
            let mut success = false;

            while attempts > 0 {
                let form = multipart::Form::new()
                    .file("file", file_path)
                    .expect("Failed to attach file")
                    .text("response_format", "text")
                    .text("model", "whisper-1");

                let response_result = client
                    .post("https://api.openai.com/v1/audio/transcriptions")
                    .header("Authorization", format!("Bearer {}", api_key))
                    .multipart(form)
                    .send();

                match response_result {
                    Ok(response) => {
                        if response.status().is_success() {
                            match response.text() {
                                Ok(result) => {
                                    transcript = result;
                                    success = true;
                                }
                                Err(e) => {
                                    transcript = format!("Failed to read response: {}", e);
                                }
                            }
                        } else {
                            let error_text = response
                                .text()
                                .unwrap_or_else(|_| "Unknown error".to_string());
                            transcript = format!("API error: {}", error_text);
                        }
                    }
                    Err(e) => {
                        transcript = format!("Request error: {}", e);
                    }
                }

                if success {
                    break;
                }

                attempts -= 1;
                if attempts > 0 {
                    std::thread::sleep(Duration::from_secs(2));
                }
            }

            copy_to_clipboard(&transcript);
            upgraded.set_transcript_text(transcript.into());
            upgraded.set_status_text(if success { "Idle" } else { "Error" }.into());
            *state = State::Stopped;
        }
        _ => {}
    }
}

/// Copies the given text to the system clipboard using xclip or wl-copy.
fn copy_to_clipboard(text: &str) {
    let clipboard_command = if which("xclip").is_ok() {
        Command::new("xclip")
            .args(&["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .spawn()
    } else {
        Command::new("wl-copy").stdin(Stdio::piped()).spawn()
    };

    let mut child = clipboard_command.expect("Failed to spawn clipboard process");

    child
        .stdin
        .as_mut()
        .expect("Failed to open clipboard stdin")
        .write_all(text.as_bytes())
        .expect("Failed to write to clipboard");

    child.wait().expect("Clipboard process wasn't running");
}

fn main() {
    let main_window = MainWindow::new().unwrap();
    let main_window_weak = main_window.as_weak();

    // Read API key from config file
    let config_path = {
        let home_dir = std::env::var("HOME").expect("HOME environment variable not set");
        PathBuf::from(home_dir)
            .join(".config")
            .join("whisper_api_key")
    };
    let api_key = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|e| {
            eprintln!("Failed to read API key: {}", e);
            String::new()
        })
        .trim()
        .to_string();

    // Set initial status based on API key presence
    if api_key.is_empty() {
        main_window.set_status_text("Error: API key missing".into());
    } else {
        main_window.set_status_text("Idle".into());
    }

    // Check if 'ask' tool is available for refine button
    let ask_exists = which("ask").is_ok();
    main_window.set_show_refine_button(ask_exists);

    let mut state = State::Stopped;

    // Handle record button press
    main_window.on_record_pressed({
        let window = main_window_weak.clone();
        move || handle_window_state_change(window.clone(), &mut state, &api_key)
    });

    // Handle refine button press
    main_window.on_refine_pressed({
        let window = main_window_weak.clone();
        move || {
            let upgraded = window.upgrade().expect("Window upgrade failed");
            upgraded.set_status_text("Refining...".into());

            let transcript = upgraded.get_transcript_text().to_string();
            let prompt = format!(
                "Refine the following transcript, keeping the original style. Remove redundancies and clean up: {}",
                transcript
            );

            let output = Command::new("ask")
                .arg(prompt)
                .output()
                .expect("Failed to execute ask");

            let refined = String::from_utf8_lossy(&output.stdout).to_string();
            copy_to_clipboard(&refined);
            upgraded.set_transcript_text(refined.into());
            upgraded.set_status_text("Idle".into());

            Command::new("ask")
                .arg("-c")
                .output()
                .expect("Failed to clean up ask");
        }
    });

    main_window.run().unwrap();
}
