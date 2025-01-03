mod color;
mod key;

use std::collections::HashSet;
use std::f64::consts::{FRAC_PI_2, PI, TAU};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use clap::Parser;
use pangocairo::cairo;

use wayrs_client::object::ObjectId;
use wayrs_client::protocol::*;
use wayrs_client::proxy::Proxy;
use wayrs_client::{global::*, EventCtx};
use wayrs_client::{Connection, IoMode};
use wayrs_protocols::wlr_layer_shell_unstable_v1::*;
use wayrs_utils::keyboard::{Keyboard, KeyboardEvent, KeyboardHandler};
use wayrs_utils::seats::{SeatHandler, Seats};
use wayrs_utils::shm_alloc::{BufferSpec, ShmAlloc};
use wayrs_utils::keyboard::xkb;

use tokio::io::{AsyncReadExt};
use tokio::fs;
use std::time::Duration;
use tokio::time::sleep;
use tokio::sync::oneshot;


#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// The name of the config file to use.
    ///
    /// By default, $XDG_CONFIG_HOME/wlr-which-key/config.yaml or
    /// ~/.config/wlr-which-key/config.yaml is used.
    ///
    /// For example, to use ~/.config/wlr-which-key/print-srceen.yaml, set this to
    /// "print-srceen". An absolute path can be used too, extension is optional.
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (tx_in, rx_in) = oneshot::channel();
    let mut isnt_killed = true;
    tokio::spawn(async {
        loop {
            match tokio::io::stdin().read_u8().await {
                Err(_) => { break }, // stdin closed
                Ok(u8) => {
                    if u8 == 0 || u8 == 10 { break; } // EOF or newline
                }
            }
        }

        let _ = tx_in.send(true);
    });
    let (tx_wlr, rx_wlr) = oneshot::channel();
    let get_isnt_killed = || -> bool { isnt_killed };
    tokio::spawn(async {
        let (mut conn, globals) = Connection::connect_and_collect_globals().unwrap();
        conn.add_registry_cb(wl_registry_cb);

        let wl_compositor: WlCompositor = globals.bind(&mut conn, 4..=6).unwrap();
        let wlr_layer_shell: ZwlrLayerShellV1 = globals.bind(&mut conn, 2).unwrap();
        let mut did_recv_stdin = false;
        let check_recv_stdin = || -> bool { did_recv_stdin };

        let seats = Seats::bind(&mut conn, &globals);
        let shm_alloc = ShmAlloc::bind(&mut conn, &globals).unwrap();

        let wl_surface = wl_compositor.create_surface_with_cb(&mut conn, wl_surface_cb);

        let layer_surface = wlr_layer_shell.get_layer_surface_with_cb(
            &mut conn,
            wl_surface,
            None,
            zwlr_layer_shell_v1::Layer::Overlay,
            wayrs_client::cstr!("zkg-wlr").into(),
            layer_surface_cb,
        );
        layer_surface.set_anchor(&mut conn, zwlr_layer_surface_v1::Anchor::empty());
        layer_surface.set_size(&mut conn, 1920, 1080);
        layer_surface.set_keyboard_interactivity(
            &mut conn,
            zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
        );
        wl_surface.commit(&mut conn);

        let mut state = State {
            shm_alloc,
            seats,
            keyboards: Vec::new(),
            outputs: Vec::new(),

            wl_surface,
            layer_surface,
            visible_on_outputs: HashSet::new(),
            surface_scale: 1,
            exit: false,
            configured: false,
            throttle_cb: None,
            throttled: false,
        };

        globals
            .iter()
            .filter(|g| g.is::<WlOutput>())
            .for_each(|g| state.bind_output(&mut conn, g));

        while !state.exit {
            conn.flush(IoMode::Blocking);
            conn.recv_events(IoMode::Blocking);
            conn.dispatch_events(&mut state);
        }
        tx_wlr.send(true);
    });

    tokio::select! {
        _ = rx_in => { () }
        _ = rx_wlr => { () }
    }
    std::process::exit(0);
}

struct State {
    shm_alloc: ShmAlloc,
    seats: Seats,
    keyboards: Vec<Keyboard>,
    outputs: Vec<Output>,

    wl_surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    visible_on_outputs: HashSet<ObjectId>,
    surface_scale: u32,
    exit: bool,
    configured: bool,
    throttle_cb: Option<WlCallback>,
    throttled: bool,
}

struct Output {
    wl: WlOutput,
    reg_name: u32,
    scale: u32,
}

impl State {
    fn draw(&mut self, conn: &mut Connection<Self>) {
        if !self.configured {
            return;
        }

        let scale = if self.wl_surface.version() >= 6 {
            self.surface_scale
        } else {
            self.outputs
                .iter()
                .filter(|o| self.visible_on_outputs.contains(&o.wl.id()))
                .map(|o| o.scale)
                .max()
                .unwrap_or(1)
        };

        let width = 1920;
        let height = 1080;

        let (buffer, canvas) = self
            .shm_alloc
            .alloc_buffer(
                conn,
                BufferSpec {
                    width: width * scale,
                    height: height * scale,
                    stride: width * 4 * scale,
                    format: wl_shm::Format::Argb8888,
                },
            )
            .expect("could not allocate frame shm buffer");

        // Damage the entire window
        self.wl_surface.damage_buffer(
            conn,
            0,
            0,
            (width * scale) as i32,
            (height * scale) as i32,
        );

        // Attach and commit to present.
        self.wl_surface.attach(conn, Some(buffer.into_wl_buffer()), 0, 0);
        self.wl_surface.commit(conn);
    }

    fn bind_output(&mut self, conn: &mut Connection<Self>, global: &Global) {
        let wl: WlOutput = global.bind_with_cb(conn, 1..=4, wl_output_cb).unwrap();
        self.outputs.push(Output {
            wl,
            reg_name: global.name,
            scale: 1,
        });
    }
}

impl SeatHandler for State {
    fn get_seats(&mut self) -> &mut Seats {
        &mut self.seats
    }

    fn keyboard_added(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        self.keyboards.push(Keyboard::new(conn, seat));
    }

    fn keyboard_removed(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        let i = self
            .keyboards
            .iter()
            .position(|k| k.seat() == seat)
            .unwrap();
        let keyboard = self.keyboards.swap_remove(i);
        keyboard.destroy(conn);
    }
}

fn processKeyEvent(isPress: bool, event: &KeyboardEvent) -> bool {
    let checkMod = |m| -> bool { event.xkb_state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE) };
    let mod_alt = checkMod(xkb::MOD_NAME_ALT);
    let mod_ctrl = checkMod(xkb::MOD_NAME_CTRL);
    let mod_shift = checkMod(xkb::MOD_NAME_SHIFT);
    let mod_meta = checkMod(xkb::MOD_NAME_LOGO);
    let sym_val = event.keysym.raw();
    let mut mod_val = 0;
    mod_val += if (mod_shift) { 1  } else { 0 };
    mod_val += if (mod_ctrl ) { 4  } else { 0 };
    mod_val += if (mod_alt  ) { 8  } else { 0 };
    mod_val += if (mod_meta ) { 64 } else { 0 };
    return match sym_val == 65307 && !isPress {
        true => true,
        _ => {
            println!("{} {} {}", sym_val, if (isPress) { "press" } else { "release" }, mod_val);
            return false;
        }
    }

}

impl KeyboardHandler for State {
    fn get_keyboard(&mut self, wl_keyboard: WlKeyboard) -> &mut Keyboard {
        self.keyboards
            .iter_mut()
            .find(|k| k.wl_keyboard() == wl_keyboard)
            .unwrap()
    }

    fn key_presed(&mut self, conn: &mut Connection<Self>, event: KeyboardEvent) {
        self.exit = processKeyEvent(true, &event);
        if (self.exit) {
            conn.break_dispatch_loop();
        }
    }

    fn key_released(&mut self, conn: &mut Connection<Self>, event: KeyboardEvent) {
        self.exit = processKeyEvent(false, &event);
        if (self.exit) {
            conn.break_dispatch_loop();
        }
    }
}

fn wl_registry_cb(conn: &mut Connection<State>, state: &mut State, event: &wl_registry::Event) {
    match event {
        wl_registry::Event::Global(g) if g.is::<WlOutput>() => state.bind_output(conn, g),
        wl_registry::Event::GlobalRemove(name) => {
            if let Some(output_i) = state.outputs.iter().position(|o| o.reg_name == *name) {
                let output = state.outputs.swap_remove(output_i);
                state.visible_on_outputs.remove(&output.wl.id());
                if output.wl.version() >= 3 {
                    output.wl.release(conn);
                }
            }
        }
        _ => (),
    }
}

fn wl_output_cb(ctx: EventCtx<State, WlOutput>) {
    if let wl_output::Event::Scale(scale) = ctx.event {
        let output = ctx
            .state
            .outputs
            .iter_mut()
            .find(|o| o.wl == ctx.proxy)
            .unwrap();
        let scale: u32 = scale.try_into().unwrap();
        if output.scale != scale {
            output.scale = scale;
        }
    }
}

fn wl_surface_cb(ctx: EventCtx<State, WlSurface>) {
    assert_eq!(ctx.proxy, ctx.state.wl_surface);
    match ctx.event {
        wl_surface::Event::Enter(output) => {
            ctx.state.visible_on_outputs.insert(output);
        }
        wl_surface::Event::Leave(output) => {
            ctx.state.visible_on_outputs.remove(&output);
        }
        _ => (),
    }
}

fn layer_surface_cb(ctx: EventCtx<State, ZwlrLayerSurfaceV1>) {
    assert_eq!(ctx.proxy, ctx.state.layer_surface);
    match ctx.event {
        zwlr_layer_surface_v1::Event::Configure(args) => {
            ctx.proxy.ack_configure(ctx.conn, args.serial);
            ctx.state.configured = true;
            ctx.state.draw(ctx.conn);
        }
        zwlr_layer_surface_v1::Event::Closed => {
            ctx.state.exit = true;
            ctx.conn.break_dispatch_loop();
        }
        _ => (),
    }
}
