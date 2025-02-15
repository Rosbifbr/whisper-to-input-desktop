use reqwest;
use reqwest::blocking::{Client, multipart};
use std::env;

slint::slint!{
    import { Button, VerticalBox, HorizontalBox } from "std-widgets.slint";
    export component MainWindow inherits Window {
        min-width: 640px;
        min-height: 480px;
        callback record-pressed <=> record.clicked;
        in-out property <string> status-text: "Initializing!";
        in-out property <string> timer-text: "00:00";
        VerticalBox {
            HorizontalBox{
                timer := Text {
                    text: timer-text;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
                status := Text {
                    text: status-text;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
            }
            record := Button {
                text: "Record";
            }
        }
    }
}

#[derive(Eq, PartialEq)]
enum State {
    Recording,
    Stopped,
    Processing,
}
fn handle_window_state_change (window: slint::Weak<MainWindow>, state: &mut State, api_key: &String) {
    use std::process::Command;
    let upgraded = window.upgrade().unwrap();
    //TODO: Replace arecord with cross-platform lib!!
    if *state == State::Stopped {
        let _child = Command::new("arecord")
            .args(&["-f", "cd", "-t", "wav", "-q", "/tmp/whisper_record.wav"])
            .spawn()
            .expect("Failed to start recording");
        *state = State::Recording;
        upgraded.set_status_text(slint::SharedString::from("Recording..."));
    }
    else if *state == State::Recording {
        Command::new("pkill")
            .arg("arecord")
            .spawn()
            .expect("Failed to stop recording");

        *state = State::Processing;
        upgraded.set_status_text(slint::SharedString::from("Processing..."));

        let client = Client::new();
        let form = reqwest::blocking::multipart::Form::new()
                .file("file", "/tmp/whisper_record.wav").unwrap()
                .text("response_format", "text")
                .text("model", "whisper-1");
        let response = client
                .post("https://api.openai.com/v1/audio/transcriptions")
                .header("Authorization", format!("Bearer {}", api_key))
                .multipart(form)
                .send()
                .expect("Failed to send transcription request");


        //After blocking period
        let transcript = response.text()
                .expect("Failed to extract transcript");

        print!("{}", transcript);
        upgraded.set_status_text(slint::SharedString::from(transcript));
        *state = State::Stopped;
    }
    else if *state == State::Processing {
        //Do nothing. This state is reset when API responds
    }
    else {
        panic!("Invalid state");
    }
}

fn main() {
    let main_window = MainWindow::new().unwrap();
    let main_window_weak = main_window.as_weak();
    let api_key = env::var("OPENAI_API_KEY").unwrap();

    let mut state = State::Stopped;
    main_window.on_record_pressed(move || {
        handle_window_state_change(main_window_weak.clone(), &mut state, &api_key);
    });

    main_window.run().unwrap();
}
