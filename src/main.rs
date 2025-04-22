use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex}; // Use Arc and Mutex for thread safety
use std::thread;
use std::time::Duration;

use reqwest::blocking::{multipart, Client};
use which::which;

slint::slint! {
    import { Button, VerticalBox, HorizontalBox, TextEdit, Spinner } from "std-widgets.slint";
    export component MainWindow inherits Window {
        min-width: 640px;
        min-height: 480px;
        callback record_pressed <=> record.clicked;
        callback refine_pressed <=> refine.clicked;
        in-out property <string> status_text: "Idle";
        in-out property <string> transcript_text: "";
        in-out property <bool> show_refine_button: true;
        in-out property <bool> processing: false; // Controls spinner visibility
        VerticalBox {
            spacing: 5px;
            padding: 5px;
            HorizontalBox {
                alignment: center;
                spinner := Spinner {
                    min-height: status.preferred-height; // Match status text height
                    min-width: self.min-height; // Make it square-ish
                    visible: processing;
                    indeterminate: true;
                }
                status := Text {
                    text: status_text;
                    horizontal-alignment: left; // Align status text left
                    vertical-alignment: center;
                }
            }
            transcript := TextEdit {
                text: transcript_text;
                read-only: true;
                vertical-stretch: 1; // Allow text edit to grow
            }

            HorizontalBox {
                alignment: center; // Center buttons
                record := Button { text: "Record"; }
                refine := Button { text: "Refine"; visible: show_refine_button; }
            }
        }
    }
}

/// Represents the current state of the application.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
enum State {
    Stopped,
    Recording,
    Processing,
}

/// Handles state transitions triggered by the record button press.
fn handle_record_button_press(
    window_weak: slint::Weak<MainWindow>,
    state_arc: Arc<Mutex<State>>, // Use Arc<Mutex<State>>
    api_key: String,
) {
    let window = match window_weak.upgrade() {
        Some(w) => w,
        None => return, // Window closed
    };

    // Lock the mutex to get exclusive access to the state.
    let mut current_state_guard = state_arc.lock().expect("Mutex poisoned");

    match *current_state_guard {
        State::Stopped => {
            println!("State Transition: Stopped -> Recording");
            *current_state_guard = State::Recording; // Update state via guard
            window.set_status_text("Recording...".into());
            window.set_processing(false); // Ensure spinner is off

            if which("arecord").is_err() {
                eprintln!("Error: 'arecord' command not found. Please install it (e.g., sudo apt install alsa-utils)");
                window.set_status_text("Error: arecord missing".into());
                *current_state_guard = State::Stopped; // Revert state
                return; // Guard dropped automatically here
            }

            // Spawn arecord
            match Command::new("arecord")
                // You might need to adjust the device (-D hw:...) depending on your system
                .args(&[
                    "-f",
                    "cd",
                    "-t",
                    "wav",
                    /*"-D", "hw:0,0",*/ "-q",
                    "/tmp/whisper_record.wav",
                ])
                .spawn()
            {
                Ok(_) => println!("arecord started successfully."),
                Err(e) => {
                    eprintln!("Failed to start recording: {}", e);
                    window.set_status_text(format!("Error starting record: {}", e).into());
                    *current_state_guard = State::Stopped; // Revert state
                }
            }
        }
        State::Recording => {
            println!("State Transition: Recording -> Processing");
            // Stop recording (best effort)
            if which("pkill").is_ok() {
                match Command::new("pkill").arg("arecord").status() {
                    Ok(status) => println!("pkill arecord exited with status: {}", status),
                    Err(e) => eprintln!("Failed to run pkill arecord: {}", e),
                }
                // Give arecord a moment to terminate and write the file
                thread::sleep(Duration::from_millis(200));
            } else {
                eprintln!("Warning: 'pkill' not found. Assuming arecord finished or was stopped manually.");
            }

            // Update UI immediately *before* dropping the lock and spawning the thread
            *current_state_guard = State::Processing;
            window.set_status_text("Processing...".into());
            window.set_processing(true); // <<-- Spinner becomes visible now!

            // ---- Release the mutex lock BEFORE spawning the thread ----
            drop(current_state_guard);
            // ----------------------------------------------------------

            // --- Background Thread ---
            let window_weak_clone = window_weak.clone();
            let state_arc_clone = state_arc.clone(); // Clone the Arc for the thread
            thread::spawn(move || {
                // This closure now owns api_key, window_weak_clone, state_arc_clone
                let file_path = "/tmp/whisper_record.wav";
                let processing_result: Result<String, String>;

                // File Checks (inside background thread)
                if !std::path::Path::new(file_path).exists() {
                    processing_result =
                        Err("Error: Recorded file /tmp/whisper_record.wav not found!".to_string());
                } else {
                    match std::fs::metadata(file_path) {
                        Ok(metadata) => {
                            let file_size = metadata.len();
                            println!("File size: {} bytes", file_size);
                            // Check size AFTER confirming existence
                            if file_size < 4096 {
                                // Heuristic for empty/corrupt WAV
                                processing_result = Err(format!("Error: Recorded file too small ({} bytes). Likely empty or recording failed.", file_size));
                            } else if file_size > 25 * 1024 * 1024 {
                                processing_result = Err(format!(
                                    "Error: Audio file too large ({} bytes). Maximum is 25 MB.",
                                    file_size
                                ));
                            } else {
                                // Network Request (inside background thread)
                                processing_result = send_to_whisper(file_path, &api_key);
                            }
                        }
                        Err(e) => {
                            processing_result =
                                Err(format!("Error accessing recorded file metadata: {}", e));
                        }
                    }
                }

                // Clean up the audio file regardless of success/failure
                let _ = std::fs::remove_file(file_path); // Ignore error if file wasn't created

                // --- Send Result Back to Main Thread ---
                slint::invoke_from_event_loop(move || {
                    // This closure runs on the main event loop thread
                    if let Some(window) = window_weak_clone.upgrade() {
                        let final_text: String;
                        let final_status: String;

                        match processing_result {
                            Ok(transcript) => {
                                println!("Transcription successful.");
                                copy_to_clipboard(&transcript);
                                final_text = transcript;
                                final_status = "Idle".to_string();
                            }
                            Err(error_message) => {
                                eprintln!("Processing failed: {}", error_message);
                                final_text = error_message.clone(); // Show error in transcript area
                                final_status = "Error".to_string();
                            }
                        }

                        window.set_transcript_text(final_text.into());
                        window.set_status_text(final_status.into());
                        window.set_processing(false); // Hide spinner

                        // Update state *on the main thread* after processing is done
                        let mut state_guard =
                            state_arc_clone.lock().expect("Mutex poisoned on callback");
                        println!("State Transition: Processing -> Stopped");
                        *state_guard = State::Stopped;
                        // Guard automatically dropped here
                    }
                })
                .expect("Failed to invoke from event loop");
            }); // --- End Background Thread ---
        }
        State::Processing => {
            println!("State: Ignored button press while Processing");
            // Do nothing, main thread still holds lock, guard dropped at end of scope
        }
    }
    // Guard dropped automatically here if not dropped earlier
}

/// Sends the audio file to Whisper API and returns the transcript or an error message.
/// Runs in the background thread.
fn send_to_whisper(file_path: &str, api_key: &str) -> Result<String, String> {
    // Build client within the function as it's not Send/Sync easily
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let mut attempts = 3;
    let mut last_error: String = "Unknown error during API call".to_string();

    while attempts > 0 {
        println!(
            "Attempting Whisper API request ({} attempts left)",
            attempts
        );

        // Recreate the form for each attempt, especially if retrying file issues
        let form = multipart::Form::new()
            .file("file", file_path)
            .map_err(|e| format!("Failed to attach file '{}': {}", file_path, e))?
            .text("response_format", "text")
            .text("model", "gpt-4o-mini-transcribe");

        let response_result = client
            .post("https://api.openai.com/v1/audio/transcriptions")
            .header("Authorization", format!("Bearer {}", api_key))
            .multipart(form)
            .send();

        match response_result {
            Ok(response) => {
                let status = response.status();
                println!("API Response Status: {}", status);
                if status.is_success() {
                    return response
                        .text() // Return Ok(transcript) directly
                        .map_err(|e| format!("Failed to read successful response body: {}", e));
                } else {
                    // Read error body for more details
                    match response.text() {
                        Ok(error_text) => {
                            last_error = format!("API error {}: {}", status, error_text);
                        }
                        Err(_) => {
                            last_error = format!("API error {} with unreadable body", status);
                        }
                    }
                    eprintln!("{}", last_error); // Log API error
                                                 // Don't retry on client errors (4xx) usually, but maybe retry on server errors (5xx)?
                    if status.is_client_error() {
                        // Specific check for 400 Bad Request, maybe file issue?
                        if status == reqwest::StatusCode::BAD_REQUEST
                            && last_error.contains("Invalid file format")
                        {
                            last_error = format!("API Error: Invalid audio file format. Ensure it's a valid WAV file. ({})", last_error);
                            // Might not want to retry this
                        } else if status == reqwest::StatusCode::UNAUTHORIZED {
                            last_error = format!(
                                "API Error: Unauthorized (401). Check your API key. ({})",
                                last_error
                            );
                            // Definitely don't retry this
                            break; // Exit retry loop
                        }
                        // Could break here for other 4xx errors too
                    }
                }
            }
            Err(e) => {
                last_error = format!("Network request error: {}", e);
                eprintln!("{}", last_error);
                if e.is_timeout() {
                    last_error = format!("Request timed out: {}", e);
                }
                // Network errors are often retryable
            }
        }

        attempts -= 1;
        if attempts > 0 {
            println!("Retrying in 2 seconds...");
            thread::sleep(Duration::from_secs(2));
        }
    }

    Err(format!(
        "Failed after multiple attempts. Last error: {}",
        last_error
    ))
}

/// Copies the given text to the system clipboard using wl-copy or xclip.
fn copy_to_clipboard(text: &str) {
    let clipboard_prog = if which("wl-copy").is_ok() {
        Some("wl-copy")
    } else if which("xclip").is_ok() {
        Some("xclip")
    } else {
        eprintln!("Warning: Neither wl-copy nor xclip found. Cannot copy to clipboard.");
        None
    };

    if let Some(prog) = clipboard_prog {
        println!("Using clipboard command: {}", prog);
        let mut command = Command::new(prog);
        if prog == "xclip" {
            command.args(&["-selection", "clipboard", "-in"]); // Use -in for piping
        }
        command.stdin(Stdio::piped());

        match command.spawn() {
            Ok(mut child) => {
                // Take ownership of stdin
                if let Some(mut stdin) = child.stdin.take() {
                    if let Err(e) = stdin.write_all(text.as_bytes()) {
                        eprintln!("Failed to write to {} stdin: {}", prog, e);
                    }
                    // stdin is dropped here, closing the pipe
                } else {
                    eprintln!("Failed to open {} stdin", prog);
                }

                // Wait for the process to finish
                match child.wait() {
                    Ok(status) => {
                        if !status.success() {
                            eprintln!("{} process exited with error: {}", prog, status);
                        } else {
                            println!("Copied to clipboard successfully.");
                        }
                    }
                    Err(e) => eprintln!("Failed to wait on {} process: {}", prog, e),
                }
            }
            Err(e) => eprintln!("Failed to spawn {} process: {}", prog, e),
        }
    }
}

fn main() {
    let main_window = MainWindow::new().unwrap();
    let main_window_weak = main_window.as_weak();

    // Read API key from config file
    let config_path = dirs::config_dir() // Use dirs crate for better path finding
        .map(|p| p.join("whisper_api_key"))
        .or_else(|| {
            eprintln!("Warning: Could not determine config directory.");
            None
        });

    let api_key = config_path.as_ref().map_or(String::new(), |path| {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|e| {
                eprintln!("Warning: Failed to read API key from {:?}: {}", path, e);
                eprintln!("Please ensure the file exists and contains your OpenAI API key.");
                String::new()
            })
    });

    // Set initial status based on API key presence
    if api_key.is_empty() {
        main_window.set_status_text("Error: API key missing or invalid".into());
        // Consider disabling the record button if the key is missing
        // main_window.global::<slint_generated::Logic>().invoke_set_record_enabled(false);
    } else {
        main_window.set_status_text("Idle".into());
    }

    // Check if 'ask' tool is available for refine button
    let ask_exists = which("ask").is_ok();
    main_window.set_show_refine_button(ask_exists);
    if !ask_exists {
        println!("'ask' command not found, hiding Refine button.");
    }

    // Use Arc<Mutex> for thread-safe shared mutable state
    let state = Arc::new(Mutex::new(State::Stopped));

    // Handle record button press
    main_window.on_record_pressed({
        let window_weak = main_window_weak.clone();
        let state_clone = state.clone(); // Clone Arc for the closure
        let api_key_clone = api_key.clone(); // Clone API key for the closure
        move || {
            if api_key_clone.is_empty() {
                if let Some(window) = window_weak.upgrade() {
                    window.set_status_text("Error: API key missing. Cannot record.".into());
                }
                return;
            }
            // Pass the cloned Arc and API key
            handle_record_button_press(
                window_weak.clone(),
                state_clone.clone(),
                api_key_clone.clone(),
            );
        }
    });

    // Handle refine button press
    main_window.on_refine_pressed({
        let window_weak = main_window_weak.clone();
        let state_clone = state.clone(); // Clone Arc for the closure
        move || {
            // Lock the mutex briefly just to check the state
            let current_state = *state_clone.lock().expect("Mutex poisoned on refine check");
            // Lock is released when guard goes out of scope here

            if current_state != State::Stopped {
                println!("Ignoring Refine press, current state: {:?}", current_state);
                 if let Some(w) = window_weak.upgrade() {
                    // Optionally provide feedback
                    // w.set_status_text("Wait for current operation".into());
                 }
                return;
            }

            // Proceed only if state is Stopped
            if let Some(upgraded) = window_weak.upgrade() {
                let transcript = upgraded.get_transcript_text().to_string();
                if transcript.is_empty() || transcript.starts_with("Error:") {
                    println!("Ignoring Refine press, no valid transcript.");
                    upgraded.set_status_text("Nothing to refine".into());
                    // Reset status back to Idle after a short delay? Maybe not needed.
                    return;
                }

                // Consider running 'ask' in a background thread too if it can be slow
                // For now, run it synchronously but show spinner
                upgraded.set_status_text("Refining...".into());
                upgraded.set_processing(true); // Show spinner for refine

                 // Need to yield to the event loop so the UI updates before blocking on 'ask'
                 // A small sleep or slint::Timer::single_shot might work, but better is a thread.
                 // Let's keep it simple for now, but be aware 'ask' might block UI update briefly.
                 // slint::Timer::single_shot(core::time::Duration::from_millis(10), move || { ... });

                let prompt = format!(
                    "Refine the following transcript, keeping the original style. Remove redundancies and clean up: {}",
                    transcript
                );

                match Command::new("ask").arg(prompt).output() {
                    Ok(output) => {
                        if output.status.success() {
                            let refined = String::from_utf8_lossy(&output.stdout).to_string();
                            copy_to_clipboard(&refined);
                            upgraded.set_transcript_text(refined.into());
                            upgraded.set_status_text("Idle".into());
                            println!("Refinement successful.");
                        } else {
                            let error_msg = String::from_utf8_lossy(&output.stderr);
                            eprintln!("'ask' command failed: {}", error_msg);
                            upgraded.set_status_text(format!("Refine failed: {}", error_msg.lines().next().unwrap_or("Unknown error")).into());
                            // Don't overwrite transcript on failure
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to execute 'ask': {}", e);
                        upgraded.set_status_text(format!("Failed to run refine: {}", e).into());
                    }
                }

                upgraded.set_processing(false); // Hide spinner after 'ask' finishes

                // Clean up 'ask' history (fire and forget)
                let _ = Command::new("ask").arg("-c").spawn();
            }
        }
    });

    println!("Application starting...");
    main_window.run().unwrap();
    println!("Application finished.");
}
