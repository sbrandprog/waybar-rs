//! Shared PipeWire helpers for the audio modules (`wireplumber`, `privacy`).
//!
//! PipeWire objects are neither `Send` nor `Sync`, so every module runs its own
//! [`MainLoopRc`] on a dedicated thread and talks to it through channels: state
//! snapshots go to the GTK thread over an [`async_channel`], and commands go
//! back through a [`pipewire::channel`] attached to the loop.
//!
//! Because an attached receiver borrows the loop, all listeners are created
//! inside the thread's `run` function and stay alive for as long as the loop
//! runs; [`spawn`] only starts the thread.
//!
//! The SPA pod helpers read and build the `Props` and `Route` parameters that
//! carry `channelVolumes` and `mute`; only `wireplumber` uses them. Hardware
//! sinks keep the effective mute on their device's `Route` (as libwireplumber's
//! `mixer-api` does), so both are needed.

use tracing::warn;

/// Start a named thread for a module's PipeWire loop.
pub fn spawn<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    if let Err(err) = std::thread::Builder::new()
        .name("waybar-pw".to_string())
        .spawn(f)
    {
        warn!("pipewire: cannot start thread: {err}");
    }
}

#[cfg(feature = "wireplumber")]
mod props {
    use std::io::Cursor;

    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::deserialize::PodDeserializer;
    use pipewire::spa::pod::serialize::PodSerializer;
    use pipewire::spa::pod::{Object, Pod, Property, Value, ValueArray};
    use pipewire::spa::sys as spa_sys;
    use pipewire::spa::utils::SpaTypes;

    /// The volume-related properties of a node's `Props` parameter.
    #[derive(Clone, Debug, Default)]
    pub struct Props {
        /// Per-channel linear gains (`channelVolumes`).
        pub volumes: Vec<f32>,
        /// A single `volume` value, used by stream nodes without channels.
        pub volume: Option<f32>,
        /// The `mute` flag, when the pod carries it.
        pub mute: Option<bool>,
        /// The linear volume step (`volumeStep`), when the pod carries it. Used
        /// to move at least one hardware step per scroll, as `mixer-api` does.
        pub step: Option<f32>,
    }

    impl Props {
        /// The effective linear gain (loudest channel, or the scalar `volume`),
        /// or `None` when the pod carries no volume. Callers must not turn
        /// "unknown" into a concrete value — a default of `1.0` shows 100%.
        pub fn linear(&self) -> Option<f32> {
            self.volumes
                .iter()
                .copied()
                .fold(None, |acc: Option<f32>, v| {
                    Some(acc.map_or(v, |a| a.max(v)))
                })
                .or(self.volume)
        }

        /// Merge the fields present in a (possibly sparse) pod update into
        /// `self`; absent fields keep their previous value.
        pub fn merge(&mut self, other: &Props) {
            if !other.volumes.is_empty() {
                self.volumes = other.volumes.clone();
            }
            if other.volume.is_some() {
                self.volume = other.volume;
            }
            if other.mute.is_some() {
                self.mute = other.mute;
            }
            if other.step.is_some() {
                self.step = other.step;
            }
        }
    }

    /// A device route, carrying the effective hardware volume/mute.
    #[derive(Clone, Debug, Default)]
    pub struct Route {
        pub index: i32,
        pub device: i32,
        pub props: Props,
    }

    fn props_from_object(object: Object) -> Props {
        let mut props = Props::default();
        for property in object.properties {
            match property.key {
                spa_sys::SPA_PROP_channelVolumes => {
                    if let Value::ValueArray(ValueArray::Float(values)) = property.value {
                        props.volumes = values;
                    }
                }
                spa_sys::SPA_PROP_volume => {
                    if let Value::Float(value) = property.value {
                        props.volume = Some(value);
                    }
                }
                spa_sys::SPA_PROP_mute => {
                    if let Value::Bool(mute) = property.value {
                        props.mute = Some(mute);
                    }
                }
                spa_sys::SPA_PROP_volumeStep => {
                    if let Value::Float(step) = property.value {
                        props.step = Some(step);
                    }
                }
                _ => {}
            }
        }
        props
    }

    fn deserialize(pod: &Pod) -> Option<Value> {
        let (_, value) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()).ok()?;
        Some(value)
    }

    /// Parse a raw `Props` pod into [`Props`].
    pub fn parse_props(pod: &Pod) -> Option<Props> {
        match deserialize(pod)? {
            Value::Object(object) => Some(props_from_object(object)),
            _ => None,
        }
    }

    /// Parse a raw `Route` pod into [`Route`].
    pub fn parse_route(pod: &Pod) -> Option<Route> {
        let Value::Object(object) = deserialize(pod)? else {
            return None;
        };
        let mut index = None;
        let mut device = None;
        let mut props = Props::default();
        for property in object.properties {
            match property.key {
                spa_sys::SPA_PARAM_ROUTE_index => {
                    if let Value::Int(value) = property.value {
                        index = Some(value);
                    }
                }
                spa_sys::SPA_PARAM_ROUTE_device => {
                    if let Value::Int(value) = property.value {
                        device = Some(value);
                    }
                }
                spa_sys::SPA_PARAM_ROUTE_props => {
                    if let Value::Object(nested) = property.value {
                        props = props_from_object(nested);
                    }
                }
                _ => {}
            }
        }
        Some(Route {
            index: index?,
            device: device?,
            props,
        })
    }

    /// Serialize a `Props` pod with `channelVolumes` and an optional `mute`.
    pub fn props_bytes(volumes: &[f32], mute: Option<bool>) -> Option<Vec<u8>> {
        let mut properties = Vec::new();
        if !volumes.is_empty() {
            properties.push(Property::new(
                spa_sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(volumes.to_vec())),
            ));
        }
        if let Some(mute) = mute {
            properties.push(Property::new(spa_sys::SPA_PROP_mute, Value::Bool(mute)));
        }
        if properties.is_empty() {
            return None;
        }
        let object = Object {
            type_: SpaTypes::ObjectParamProps.as_raw(),
            id: ParamType::Props.as_raw(),
            properties,
        };
        serialize(&Value::Object(object))
    }

    /// Serialize a `Route` pod that writes `channelVolumes`/`mute` for a route.
    pub fn route_bytes(
        index: i32,
        device: i32,
        volumes: &[f32],
        mute: Option<bool>,
    ) -> Option<Vec<u8>> {
        let mut properties = Vec::new();
        if !volumes.is_empty() {
            properties.push(Property::new(
                spa_sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(volumes.to_vec())),
            ));
        }
        if let Some(mute) = mute {
            properties.push(Property::new(spa_sys::SPA_PROP_mute, Value::Bool(mute)));
        }
        let props = Object {
            type_: SpaTypes::ObjectParamProps.as_raw(),
            id: ParamType::Props.as_raw(),
            properties,
        };
        let route = Object {
            type_: SpaTypes::ObjectParamRoute.as_raw(),
            id: ParamType::Route.as_raw(),
            properties: vec![
                Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(index)),
                Property::new(spa_sys::SPA_PARAM_ROUTE_device, Value::Int(device)),
                Property::new(spa_sys::SPA_PARAM_ROUTE_props, Value::Object(props)),
                Property::new(spa_sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
            ],
        };
        serialize(&Value::Object(route))
    }

    fn serialize(value: &Value) -> Option<Vec<u8>> {
        let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), value).ok()?;
        Some(cursor.into_inner())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn props_roundtrip() {
            let bytes = props_bytes(&[0.25, 0.25], Some(true)).expect("serialize");
            let pod = Pod::from_bytes(&bytes).expect("valid pod");
            let props = parse_props(pod).expect("parse");
            assert_eq!(props.volumes, vec![0.25, 0.25]);
            assert_eq!(props.mute, Some(true));
            assert_eq!(props.linear(), Some(0.25));
        }

        #[test]
        fn props_mute_only() {
            let bytes = props_bytes(&[], Some(false)).expect("serialize");
            let pod = Pod::from_bytes(&bytes).expect("valid pod");
            let props = parse_props(pod).expect("parse");
            assert!(props.volumes.is_empty());
            assert_eq!(props.mute, Some(false));
            assert_eq!(props.linear(), None);
        }

        #[test]
        fn props_merge_is_sparse() {
            let mut props = Props {
                volumes: vec![0.125, 0.125],
                volume: None,
                mute: Some(true),
                step: None,
            };
            // A mute-only update must not drop the known volumes.
            props.merge(&Props {
                volumes: Vec::new(),
                volume: None,
                mute: Some(false),
                step: None,
            });
            assert_eq!(props.volumes, vec![0.125, 0.125]);
            assert_eq!(props.mute, Some(false));
        }

        #[test]
        fn route_roundtrip() {
            let bytes = route_bytes(3, 4, &[0.125, 0.125], Some(true)).expect("serialize");
            let pod = Pod::from_bytes(&bytes).expect("valid pod");
            let route = parse_route(pod).expect("parse");
            assert_eq!(route.index, 3);
            assert_eq!(route.device, 4);
            assert_eq!(route.props.volumes, vec![0.125, 0.125]);
            assert_eq!(route.props.mute, Some(true));
        }
    }
}

#[cfg(feature = "wireplumber")]
pub use props::{Props, parse_props, parse_route, props_bytes, route_bytes};
