use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use gtk::glib;

use crate::THREAD_POOL;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MsaiAudioDeviceClass")]
pub enum AudioDeviceClass {
    #[default]
    Source,
    Sink,
}

impl AudioDeviceClass {
    fn for_str(string: &str) -> Option<Self> {
        match string {
            "Audio/Source" => Some(Self::Source),
            "Audio/Sink" => Some(Self::Sink),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Source => "Audio/Source",
            Self::Sink => "Audio/Sink",
        }
    }
}

pub async fn find_default_name(class: AudioDeviceClass) -> Result<String> {
    match THREAD_POOL
        .push_future(move || find_default_name_gst(class))
        .context("Failed to push future to main thread pool")?
        .await
    {
        Ok(res) => Ok(res),
        Err(err) => {
            log::warn!("Failed to find default name using gstreamer: {:?}", err);
            log::debug!("Manually using libpulse instead");

            let pa_context = pa::Context::connect().await?;
            pa_context.find_default_device_name(class).await
        }
    }
}

fn find_default_name_gst(class: AudioDeviceClass) -> Result<String> {
    let device_monitor = gst::DeviceMonitor::new();
    device_monitor.add_filter(Some(class.as_str()), None);

    device_monitor.start()?;
    let devices = device_monitor.devices();
    device_monitor.stop();

    log::debug!("Finding device name for class `{:?}`", class);

    for device in devices {
        let device_class = match AudioDeviceClass::for_str(&device.device_class()) {
            Some(device_class) => device_class,
            None => {
                log::debug!(
                    "Skipping device `{}` as it has unknown device class `{}`",
                    device.name(),
                    device.device_class()
                );
                continue;
            }
        };

        if device_class != class {
            continue;
        }

        let properties = match device.properties() {
            Some(properties) => properties,
            None => {
                log::warn!(
                    "Skipping device `{}` as it has no properties",
                    device.name()
                );
                continue;
            }
        };

        let is_default = match properties.get::<bool>("is-default") {
            Ok(is_default) => is_default,
            Err(err) => {
                log::warn!(
                    "Skipping device `{}` as it has no `is-default` property. {:?}",
                    device.name(),
                    err
                );
                continue;
            }
        };

        if !is_default {
            log::debug!(
                "Skipping device `{}` as it is not the default",
                device.name()
            );
            continue;
        }

        let mut node_name = match properties.get::<String>("node.name") {
            Ok(node_name) => node_name,
            Err(err) => {
                log::warn!(
                    "Skipping device `{}` as it has no node.name property. {:?}",
                    device.name(),
                    err
                );
                continue;
            }
        };

        if device_class == AudioDeviceClass::Sink {
            node_name.push_str(".monitor");
        }

        return Ok(node_name);
    }

    Err(anyhow!("Failed to find a default device"))
}

mod pa {
    use anyhow::{bail, Context as ErrContext, Result};
    use futures_channel::{mpsc, oneshot};
    use futures_util::{
        future::{self, Either},
        StreamExt,
    };
    use gtk::glib;
    use pulse::{
        context::{Context as ContextInner, FlagSet, State},
        proplist::{properties, Proplist},
    };

    use std::{fmt, time::Duration};

    use super::AudioDeviceClass;
    use crate::config::APP_ID;

    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

    pub struct Context {
        inner: ContextInner,

        // `ContextInner` does not hold a reference causing it
        // to be freed and cause error after `Context::connect`.
        #[allow(dead_code)]
        main_loop: pulse_glib::Mainloop,
    }

    impl fmt::Debug for Context {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Context")
        }
    }

    impl Context {
        pub async fn connect() -> Result<Self> {
            let main_loop =
                pulse_glib::Mainloop::new(None).context("Failed to create pulse Mainloop")?;

            let mut proplist = Proplist::new().unwrap();
            proplist
                .set_str(properties::APPLICATION_ID, APP_ID)
                .unwrap();
            proplist
                .set_str(properties::APPLICATION_NAME, "Kooha")
                .unwrap();

            let mut inner = ContextInner::new_with_proplist(&main_loop, APP_ID, &proplist)
                .context("Failed to create pulse Context")?;

            inner.connect(None, FlagSet::NOFLAGS, None)?;

            let (mut tx, mut rx) = mpsc::channel(1);

            inner.set_state_callback(Some(Box::new(move || {
                let _ = tx.start_send(());
            })));

            log::debug!("Waiting for context server connection");

            while rx.next().await.is_some() {
                match inner.get_state() {
                    State::Ready => break,
                    State::Failed => bail!("Connection failed or disconnected"),
                    State::Terminated => bail!("Connection context terminated"),
                    _ => {}
                }
            }

            log::debug!("Connected context to server");

            inner.set_state_callback(None);

            Ok(Self { inner, main_loop })
        }

        pub async fn find_default_device_name(&self, class: AudioDeviceClass) -> Result<String> {
            let (tx, rx) = oneshot::channel();
            let mut tx = Some(tx);

            let mut operation = self.inner.introspect().get_server_info(move |server_info| {
                let tx = if let Some(tx) = tx.take() {
                    tx
                } else {
                    log::error!("Called get_server_info twice!");
                    return;
                };

                match class {
                    AudioDeviceClass::Source => {
                        let _ = tx.send(
                            server_info
                                .default_source_name
                                .as_ref()
                                .map(|s| s.to_string()),
                        );
                    }
                    AudioDeviceClass::Sink => {
                        let _ = tx.send(
                            server_info
                                .default_sink_name
                                .as_ref()
                                .map(|s| format!("{}.monitor", s)),
                        );
                    }
                }
            });

            let name = match future::select(rx, glib::timeout_future(DEFAULT_TIMEOUT)).await {
                Either::Left((name, _)) => name,
                Either::Right(_) => {
                    operation.cancel();
                    bail!("get_server_info operation timeout reached");
                }
            };

            name.unwrap().context("Found no default device")
        }
    }

    impl Drop for Context {
        fn drop(&mut self) {
            self.inner.disconnect();
        }
    }
}