#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Deserialize;
use std::sync::mpsc::{channel, Receiver, Sender};

/// Input event sent by the remote client (x/y normalized 0..1, k = Windows virtual-key code).
#[derive(Deserialize, Debug)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Input {
    Move { x: f64, y: f64 },
    Down { b: String },
    Up { b: String },
    Wheel { dy: f64 },
    Kd { k: u32 },
    Ku { k: u32 },
}

#[tauri::command]
fn input(ev: Input, tx: tauri::State<Sender<Input>>) {
    let _ = tx.send(ev);
}

fn button(b: &str) -> Button {
    match b {
        "right" => Button::Right,
        "middle" => Button::Middle,
        _ => Button::Left,
    }
}

/// Owns the Enigo instance on one thread; replays events in order.
fn input_loop(rx: Receiver<Input>) {
    // Debug/test switch: log events instead of moving the real mouse/keyboard.
    if std::env::var_os("PCLONE_DRY_INPUT").is_some() {
        for ev in rx {
            eprintln!("INPUT {ev:?}");
        }
        return;
    }
    let mut e = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(err) => return eprintln!("input disabled: {err}"),
    };
    for ev in rx {
        let res = match ev {
            Input::Move { x, y } => {
                let (w, h) = e.main_display().unwrap_or((1920, 1080));
                e.move_mouse((x.clamp(0.0, 1.0) * w as f64) as i32, (y.clamp(0.0, 1.0) * h as f64) as i32, Coordinate::Abs)
            }
            Input::Down { b } => e.button(button(&b), Direction::Press),
            Input::Up { b } => e.button(button(&b), Direction::Release),
            Input::Wheel { dy } => {
                // ponytail: ~100 deltaY per wheel notch; tune divisor if scrolling feels off
                let n = (dy / 100.0).round() as i32;
                e.scroll(if n == 0 { dy.signum() as i32 } else { n }, Axis::Vertical)
            }
            Input::Kd { k } => e.key(Key::Other(k), Direction::Press),
            Input::Ku { k } => e.key(Key::Other(k), Direction::Release),
        };
        if let Err(err) = res {
            eprintln!("input failed: {err}");
        }
    }
}

fn main() {
    let (tx, rx) = channel();
    std::thread::spawn(move || input_loop(rx));
    tauri::Builder::default()
        .manage(tx)
        .invoke_handler(tauri::generate_handler![input])
        .run(tauri::generate_context!())
        .expect("failed to start app");
}
