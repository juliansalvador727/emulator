pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod joypad;
pub mod opcodes;
pub mod ppu;
pub mod render;
pub mod trace;

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate bitflags;

use bus::Bus;
use cartridge::Rom;
use cpu::CPU;
use joypad::Joypad;
use joypad::JoypadButton;
use ppu::NesPPU;
use render::frame::Frame;
use render::show_tile;
use trace::trace;

use std::collections::HashMap;

use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;

// Runs a game ROM, rendering the PPU background to an SDL2 window.
// The bus fires the callback once per frame (at vblank); we render the
// nametable into a Frame and blit it. Loads game.nes from the working dir.
fn run_game() {
    let sdl_context = sdl2::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window = video_subsystem
        .window("NES game", (256.0 * 3.0) as u32, (240.0 * 3.0) as u32)
        .position_centered()
        .build()
        .unwrap();

    let mut canvas = window.into_canvas().present_vsync().build().unwrap();
    let mut event_pump = sdl_context.event_pump().unwrap();
    canvas.set_scale(3.0, 3.0).unwrap();

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormatEnum::RGB24, 256, 240)
        .unwrap();

    let bytes: Vec<u8> = std::fs::read("game.nes").unwrap();
    let rom = Rom::new(&bytes).unwrap();

    let mut frame = Frame::new();

    // Keyboard -> NES controller button mapping.
    let mut key_map = HashMap::new();
    key_map.insert(Keycode::Down, JoypadButton::DOWN);
    key_map.insert(Keycode::Up, JoypadButton::UP);
    key_map.insert(Keycode::Right, JoypadButton::RIGHT);
    key_map.insert(Keycode::Left, JoypadButton::LEFT);
    key_map.insert(Keycode::Space, JoypadButton::SELECT);
    key_map.insert(Keycode::Return, JoypadButton::START);
    key_map.insert(Keycode::A, JoypadButton::BUTTON_A);
    key_map.insert(Keycode::S, JoypadButton::BUTTON_B);

    // Called by the bus at each vblank: draw the background, present it,
    // and drain the SDL event queue (updating the joypad) so the window
    // stays responsive.
    let bus = Bus::new(rom, move |ppu: &NesPPU, joypad: &mut Joypad| {
        render::render(ppu, &mut frame);
        texture.update(None, &frame.data, 256 * 3).unwrap();
        canvas.copy(&texture, None, None).unwrap();
        canvas.present();

        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => std::process::exit(0),
                Event::KeyDown { keycode, .. } => {
                    if let Some(button) = keycode.and_then(|k| key_map.get(&k)) {
                        joypad.set_button_pressed_status(*button, true);
                    }
                }
                Event::KeyUp { keycode, .. } => {
                    if let Some(button) = keycode.and_then(|k| key_map.get(&k)) {
                        joypad.set_button_pressed_status(*button, false);
                    }
                }
                _ => { /* do nothing */ }
            }
        }
    });

    let mut cpu = CPU::new(bus);
    cpu.reset();
    cpu.run();
}

// Runs the nestest ROM in automation mode, printing a CPU trace per instruction.
// Redirect stdout to a file and diff against nestest.log to validate the CPU.
fn run_nestest() {
    let bytes: Vec<u8> = std::fs::read("nestest.nes").unwrap();
    let rom = Rom::new(&bytes).unwrap();

    let bus = Bus::new(rom, |_, _| {});
    let mut cpu = CPU::new(bus);
    cpu.reset();
    cpu.program_counter = 0xC000;

    cpu.run_with_callback(move |cpu| {
        println!("{}", trace(cpu));
    });
}

// Renders a single CHR tile to an SDL2 window (the ch6.3 deliverable).
// Pass any iNES ROM that ships CHR ROM (e.g. pacman.nes); defaults to
// nestest.nes. snake.nes has 0 bytes of CHR and will panic.
fn run_tiles(rom_path: &str) {
    let sdl_context = sdl2::init().unwrap();
    let video_subsystem = sdl_context.video().unwrap();
    let window = video_subsystem
        .window("Tile viewer", (256.0 * 3.0) as u32, (240.0 * 3.0) as u32)
        .position_centered()
        .build()
        .unwrap();

    let mut canvas = window.into_canvas().present_vsync().build().unwrap();
    let mut event_pump = sdl_context.event_pump().unwrap();
    canvas.set_scale(3.0, 3.0).unwrap();

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_target(PixelFormatEnum::RGB24, 256, 240)
        .unwrap();

    let bytes: Vec<u8> = std::fs::read(rom_path).unwrap();
    let rom = Rom::new(&bytes).unwrap();

    let tile_frame = show_tile(&rom.chr_rom, 1, 0);

    texture.update(None, &tile_frame.data, 256 * 3).unwrap();
    canvas.copy(&texture, None, None).unwrap();
    canvas.present();

    loop {
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => std::process::exit(0),
                _ => { /* do nothing */ }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("nestest") => run_nestest(),
        Some("tiles") => run_tiles(args.get(2).map(|s| s.as_str()).unwrap_or("nestest.nes")),
        _ => run_game(),
    }
}
