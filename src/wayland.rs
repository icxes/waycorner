use crate::{
    config::{CornerConfig, Location},
    corner::Corner,
};
use anyhow::{Context, Result};

use crossbeam_utils::thread;
use smithay_client_toolkit::shm::Format;
use smithay_client_toolkit::{
    data_device::DataDeviceHandler,
    default_environment,
    environment::{Environment, SimpleGlobal},
    output::{with_output_info, OutputInfo, XdgOutputHandler},
    primary_selection::PrimarySelectionHandler,
    seat,
};
use std::{
    convert::TryInto,
    io::{BufWriter, Seek, SeekFrom, Write},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
};

use wayland_client::{
    protocol::{wl_output::WlOutput, wl_pointer, wl_surface::WlSurface},
    Attached, Display, Main, Proxy,
};
use wayland_protocols::{
    unstable::xdg_output::v1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1,
    wlr::unstable::layer_shell::v1::client::{
        zwlr_layer_shell_v1,
        zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
    },
};
default_environment!(Waycorner,  fields = [
    layer_shell: SimpleGlobal<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    sctk_xdg_out: XdgOutputHandler,
],
singles = [
    zwlr_layer_shell_v1::ZwlrLayerShellV1 => layer_shell,
    ZxdgOutputManagerV1 => sctk_xdg_out,
],);

struct GlobalState {
    close_requested: bool,
}

pub struct Wayland {
    pub preview: bool,
    corner_to_surfaces: Vec<(Corner, Vec<WlSurface>)>,
}

const RED: u32 = 0xD0_FF_00_00;
const TRANSPARENT: u32 = 0x00_00_00_00;

impl Wayland {
    pub fn new(configs: Vec<CornerConfig>, preview: bool) -> Self {
        Wayland {
            preview,
            corner_to_surfaces: configs
                .into_iter()
                .map(|corner| (Corner::new(corner), vec![]))
                .collect(),
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let display = Display::connect_to_env()?;
        let mut event_queue = display.create_event_queue();
        let wl_display = Proxy::clone(&display).attach(event_queue.token());

        let (sctk_outputs, sctk_xdg_out) = XdgOutputHandler::new_output_handlers();

        let mut seat_handler = smithay_client_toolkit::seat::SeatHandler::new();
        let sctk_data_device_manager = DataDeviceHandler::init(&mut seat_handler);
        let sctk_primary_selection_manager = PrimarySelectionHandler::init(&mut seat_handler);

        let environment = smithay_client_toolkit::environment::Environment::new(
            &wl_display,
            &mut event_queue,
            Waycorner {
                sctk_compositor: SimpleGlobal::new(),
                sctk_shm: smithay_client_toolkit::shm::ShmHandler::new(),
                sctk_seats: seat_handler,
                sctk_outputs,
                sctk_xdg_out,
                sctk_subcompositor: SimpleGlobal::new(),
                sctk_data_device_manager,
                sctk_primary_selection_manager,
                layer_shell: SimpleGlobal::new(),
            },
        )?;

        let layer_shell = environment.require_global::<zwlr_layer_shell_v1::ZwlrLayerShellV1>();
        let env_handle = environment.clone();

        for output in environment.get_all_outputs() {
            if let Some(info) = with_output_info(&output, Clone::clone) {
                self.output_handler(&env_handle, &layer_shell, output, &info)?;
            }
        }

        let (tx, rx): (Sender<wl_pointer::Event>, Receiver<wl_pointer::Event>) = mpsc::channel();

        for seat in environment.get_all_seats() {
            let filter_tx = tx.clone();
            if let Some(has_ptr) = seat::with_seat_data(&seat, |seat_data| {
                seat_data.has_pointer && !seat_data.defunct
            }) {
                if !has_ptr {
                    continue;
                }

                seat.get_pointer().quick_assign(move |_, event, _| {
                    filter_tx
                        .send(event)
                        .expect("could not send event on channel");
                });
            }
        }

        let pointer_event_receiver = Arc::new(Mutex::new(rx));
        thread::scope(|scope| -> Result<()> {
            scope.spawn(|_| loop {
                let event = pointer_event_receiver
                    .lock()
                    .expect("Could not lock event receiver")
                    .recv();
                match event {
                    Ok(wl_pointer::Event::Enter { surface, .. }) => {
                        self.get_corner(&surface)
                            .and_then(|corner| corner.on_enter_mouse().ok());
                    }
                    Ok(wl_pointer::Event::Leave { surface, .. }) => {
                        self.get_corner(&surface)
                            .and_then(|corner| corner.on_leave_mouse().ok());
                    }
                    _ => (),
                }
            });

            self.corner_to_surfaces.iter().for_each(|(corner, _)| {
                scope.spawn(move |_| loop {
                    corner.wait().unwrap();
                });
            });

            let mut global_state = GlobalState {
                close_requested: false,
            };

            loop {
                event_queue
                    .dispatch(&mut global_state, |_, _, _| {
                        panic!("An event was received not assigned to any callback!")
                    })
                    .context("Wayland connection lost!")?;

                if global_state.close_requested {
                    break;
                }
            }
            Ok(())
        })
        .unwrap()
    }

    fn get_corner(&self, surface: &WlSurface) -> Option<&Corner> {
        self.corner_to_surfaces
            .iter()
            .filter(|(_, surfaces)| surfaces.iter().any(|value| value == surface))
            .map(|(corner, _)| corner)
            .next()
    }

    fn output_handler(
        &mut self,
        environment: &Environment<Waycorner>,
        layer_shell: &Attached<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
        output: WlOutput,
        info: &OutputInfo,
    ) -> Result<()> {
        info!("{:?}", info);
        let preview = self.preview;
        self.corner_to_surfaces
            .iter_mut()
            .map(|(corner, surfaces)| -> Result<()> {
                debug!("{:?}", corner);
                if !corner.is_match(info.description.as_str()) {
                    debug!("Output description is NOT a match");
                    return Ok(());
                }
                debug!("Output description IS a match");

                if info.obsolete {
                    debug!("Clearing surfaces");
                    surfaces.clear();
                    return Ok(());
                }

                debug!("Adding surfaces");
                let mut corner_surfaces = Wayland::corner_setup(
                    environment,
                    layer_shell,
                    &output,
                    corner.config.clone(),
                    preview,
                )?;

                surfaces.append(&mut corner_surfaces);
                Ok(())
            })
            .collect::<Result<_>>()?;
        if info.obsolete {
            info!("Releasing output");
            output.release();
        }
        Ok(())
    }

    fn corner_setup(
        environment: &Environment<Waycorner>,
        layer_shell: &Attached<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
        output: &WlOutput,
        corner_config: CornerConfig,
        preview: bool,
    ) -> Result<Vec<WlSurface>> {
        corner_config
            .locations
            .iter()
            .map(|location| match location {
                Location::TopLeft => Anchor::Top | Anchor::Left,
                Location::TopRight => Anchor::Top | Anchor::Right,
                Location::BottomRight => Anchor::Bottom | Anchor::Right,
                Location::BottomLeft => Anchor::Bottom | Anchor::Left,
                Location::Left => Anchor::Left,
                Location::Right => Anchor::Right,
                Location::Top => Anchor::Top,
                Location::Bottom => Anchor::Bottom,
            })
            .map(|anchor| {
                info!("Adding anchorpoint {:?}", anchor);
                let surface = environment.create_surface().detach();

                let layer_surface = layer_shell.get_layer_surface(
                    &surface,
                    Some(&output),
                    zwlr_layer_shell_v1::Layer::Top,
                    "waycorner".to_owned(),
                );
                
                let size_width = 
                	if corner_config.size_width != 10 {
                		corner_config.size_width.into()
                	} else {
                		corner_config.size.into()
                	};
                
                let size_height =
                	if corner_config.size_height != 10 {
                		corner_config.size_height.into()
                	} else {
                		corner_config.size.into()
                	};
                
                layer_surface.set_size(size_width, size_height);
                layer_surface.set_anchor(anchor);
                // Ignore exclusive zones.
                layer_surface.set_exclusive_zone(-1);

                Wayland::initial_draw(environment, surface.clone(), layer_surface, preview)?;

                Ok(surface)
            })
            .collect()
    }

    fn initial_draw(
        environment: &Environment<Waycorner>,
        surface: WlSurface,
        layer_surface: Main<ZwlrLayerSurfaceV1>,
        preview: bool,
    ) -> Result<()> {
        let mut double_pool = environment
            .create_double_pool(|_| {})
            .context("Failed to create double pool!")?;

        let surface_handle = surface.clone();

        layer_surface.quick_assign(move |layer_surface, event, _| match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                if let Some(pool) = double_pool.pool() {
                    let pxcount = width * height;
                    let bytecount = 4 * pxcount;

                    pool.resize(bytecount.try_into().unwrap()).unwrap();
                    pool.seek(SeekFrom::Start(0)).unwrap();
                    {
                        let mut writer = BufWriter::new(&mut *pool);
                        let color = if preview { RED } else { TRANSPARENT };
                        for _ in 0..pxcount {
                            writer.write_all(&color.to_ne_bytes()).unwrap();
                        }
                        writer.flush().unwrap();
                    }

                    let buffer = pool.buffer(
                        0,
                        width.try_into().unwrap(),
                        height.try_into().unwrap(),
                        (4 * width).try_into().unwrap(),
                        Format::Argb8888,
                    );
                    surface_handle.attach(Some(&buffer), 0, 0);
                    surface_handle.damage_buffer(
                        0,
                        0,
                        width.try_into().unwrap(),
                        height.try_into().unwrap(),
                    );
                    surface_handle.commit();
                }
            }
            _ => {}
        });

        surface.commit();
        Ok(())
    }
}
