use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{debug, error};
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use std::sync::Mutex;
use std::u32;

use once_cell::sync::Lazy;

use crate::ndisys;

use crate::ndisrcmeta;
use crate::Buffer;
use crate::Receiver;
use crate::ReceiverControlHandle;
use crate::ReceiverItem;
use crate::RecvColorFormat;
use crate::TimestampMode;
use crate::DEFAULT_RECEIVER_NDI_NAME;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ndisrc",
        gst::DebugColorFlags::empty(),
        Some("NewTek NDI Source"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    ndi_name: Option<String>,
    url_address: Option<String>,
    connect_timeout: u32,
    timeout: u32,
    max_queue_length: u32,
    receiver_ndi_name: String,
    bandwidth: ndisys::NDIlib_recv_bandwidth_e,
    color_format: RecvColorFormat,
    timestamp_mode: TimestampMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ndi_name: None,
            url_address: None,
            receiver_ndi_name: DEFAULT_RECEIVER_NDI_NAME.clone(),
            connect_timeout: 10000,
            timeout: 5000,
            max_queue_length: 10,
            bandwidth: ndisys::NDIlib_recv_bandwidth_highest,
            color_format: RecvColorFormat::UyvyBgra,
            timestamp_mode: TimestampMode::ReceiveTimeTimecode,
        }
    }
}

struct State {
    video_info: Option<crate::VideoInfo>,
    video_caps: Option<gst::Caps>,
    audio_info: Option<crate::AudioInfo>,
    audio_caps: Option<gst::Caps>,
    current_latency: Option<gst::ClockTime>,
    receiver: Option<Receiver>,
}

impl Default for State {
    fn default() -> State {
        State {
            video_info: None,
            video_caps: None,
            audio_info: None,
            audio_caps: None,
            current_latency: gst::ClockTime::NONE,
            receiver: None,
        }
    }
}

pub struct NdiSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    receiver_controller: Mutex<Option<ReceiverControlHandle>>,
}

#[glib::object_subclass]
impl ObjectSubclass for NdiSrc {
    const NAME: &'static str = "NdiSrc";
    type Type = super::NdiSrc;
    type ParentType = gst_base::BaseSrc;

    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            receiver_controller: Mutex::new(None),
        }
    }
}

impl ObjectImpl for NdiSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("ndi-name")
                    .nick("NDI Name")
                    .blurb("NDI stream name of the sender")
                    .default_value(None)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("url-address")
                    .nick("URL/Address")
                    .blurb("URL/address and port of the sender, e.g. 127.0.0.1:5961")
                    .default_value(None)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("receiver-ndi-name")
                    .nick("Receiver NDI Name")
                    .blurb("NDI stream name of this receiver")
                    .default_value(Some(DEFAULT_RECEIVER_NDI_NAME.as_str()))
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecUInt::builder("connect-timeout")
                    .nick("Connect Timeout")
                    .blurb("Connection timeout in ms")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(10000)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Receive timeout in ms")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(5000)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecUInt::builder("max-queue-length")
                    .nick("Max Queue Length")
                    .blurb("Maximum receive queue length")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(10)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecInt::builder("bandwidth")
                    .nick("Bandwidth")
                    .blurb("Bandwidth, -10 metadata-only, 10 audio-only, 100 highest")
                    .minimum(-10)
                    .maximum(100)
                    .default_value(100)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("color-format", RecvColorFormat::UyvyBgra)
                    .nick("Color Format")
                    .blurb("Receive color format")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("timestamp-mode", TimestampMode::ReceiveTimeTimecode)
                    .nick("Timestamp Mode")
                    .blurb("Timestamp information to use for outgoing PTS")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        // Initialize live-ness and notify the base class that
        // we'd like to operate in Time format
        let obj = self.obj();
        obj.set_live(true);
        obj.set_format(gst::Format::Time);
    }

    fn set_property(
        &self,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let obj= self.obj();
        match pspec.name() {
            "ndi-name" => {
                let mut settings = self.settings.lock().unwrap();
                let ndi_name = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing ndi-name from {:?} to {:?}",
                    settings.ndi_name,
                    ndi_name,
                );
                settings.ndi_name = ndi_name;
            }
            "url-address" => {
                let mut settings = self.settings.lock().unwrap();
                let url_address = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing url-address from {:?} to {:?}",
                    settings.url_address,
                    url_address,
                );
                settings.url_address = url_address;
            }
            "receiver-ndi-name" => {
                let mut settings = self.settings.lock().unwrap();
                let receiver_ndi_name = value.get::<Option<String>>().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing receiver-ndi-name from {:?} to {:?}",
                    settings.receiver_ndi_name,
                    receiver_ndi_name,
                );
                settings.receiver_ndi_name =
                    receiver_ndi_name.unwrap_or_else(|| DEFAULT_RECEIVER_NDI_NAME.clone());
            }
            "connect-timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let connect_timeout = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing connect-timeout from {} to {}",
                    settings.connect_timeout,
                    connect_timeout,
                );
                settings.connect_timeout = connect_timeout;
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing timeout from {} to {}",
                    settings.timeout,
                    timeout,
                );
                settings.timeout = timeout;
            }
            "max-queue-length" => {
                let mut settings = self.settings.lock().unwrap();
                let max_queue_length = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing max-queue-length from {} to {}",
                    settings.max_queue_length,
                    max_queue_length,
                );
                settings.max_queue_length = max_queue_length;
            }
            "bandwidth" => {
                let mut settings = self.settings.lock().unwrap();
                let bandwidth = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing bandwidth from {} to {}",
                    settings.bandwidth,
                    bandwidth,
                );
                settings.bandwidth = bandwidth;
            }
            "color-format" => {
                let mut settings = self.settings.lock().unwrap();
                let color_format = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing color format from {:?} to {:?}",
                    settings.color_format,
                    color_format,
                );
                settings.color_format = color_format;
            }
            "timestamp-mode" => {
                let mut settings = self.settings.lock().unwrap();
                let timestamp_mode = value.get().unwrap();
                debug!(
                    CAT,
                    obj = obj,
                    "Changing timestamp mode from {:?} to {:?}",
                    settings.timestamp_mode,
                    timestamp_mode
                );
                if settings.timestamp_mode != timestamp_mode {
                    let _ = obj.post_message(gst::message::Latency::builder().src(&*obj).build());
                }
                settings.timestamp_mode = timestamp_mode;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "ndi-name" => {
                let settings = self.settings.lock().unwrap();
                settings.ndi_name.to_value()
            }
            "url-address" => {
                let settings = self.settings.lock().unwrap();
                settings.url_address.to_value()
            }
            "receiver-ndi-name" => {
                let settings = self.settings.lock().unwrap();
                settings.receiver_ndi_name.to_value()
            }
            "connect-timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.connect_timeout.to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            "max-queue-length" => {
                let settings = self.settings.lock().unwrap();
                settings.max_queue_length.to_value()
            }
            "bandwidth" => {
                let settings = self.settings.lock().unwrap();
                settings.bandwidth.to_value()
            }
            "color-format" => {
                let settings = self.settings.lock().unwrap();
                settings.color_format.to_value()
            }
            "timestamp-mode" => {
                let settings = self.settings.lock().unwrap();
                settings.timestamp_mode.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for NdiSrc {}

impl ElementImpl for NdiSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
            "NewTek NDI Source",
            "Source/Audio/Video/Network",
            "NewTek NDI source",
            "Ruben Gonzalez <rubenrua@teltek.es>, Daniel Vilar <daniel.peiteado@teltek.es>, Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-ndi").build(),
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::PausedToPlaying => {
                if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
                    controller.set_playing(true);
                }
            }
            gst::StateChange::PlayingToPaused => {
                if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
                    controller.set_playing(false);
                }
            }
            gst::StateChange::PausedToReady => {
                if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
                    controller.shutdown();
                }
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}

impl BaseSrcImpl for NdiSrc {
    fn negotiate(&self) -> Result<(), gst::LoggableError> {
        self.obj()
            .set_caps(&gst::Caps::builder("application/x-ndi").build())
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate caps",))
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        debug!(CAT, obj = self.obj(), "Unlocking",);
        if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
            controller.set_flushing(true);
        }
        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        debug!(CAT, obj = self.obj(), "Stop unlocking",);
        if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
            controller.set_flushing(false);
        }
        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let element = self.obj();
        *self.state.lock().unwrap() = Default::default();
        let settings = self.settings.lock().unwrap().clone();

        if settings.ndi_name.is_none() && settings.url_address.is_none() {
            return Err(gst::error_msg!(
                gst::LibraryError::Settings,
                ["No NDI name or URL/address given"]
            ));
        }

        let receiver = Receiver::connect(
            element.upcast_ref(),
            settings.ndi_name.as_deref(),
            settings.url_address.as_deref(),
            &settings.receiver_ndi_name,
            settings.connect_timeout,
            settings.bandwidth,
            settings.color_format.into(),
            settings.timestamp_mode,
            settings.timeout,
            settings.max_queue_length as usize,
        );

        match receiver {
            None => Err(gst::error_msg!(
                gst::ResourceError::NotFound,
                ["Could not connect to this source"]
            )),
            Some(receiver) => {
                *self.receiver_controller.lock().unwrap() =
                    Some(receiver.receiver_control_handle());
                let mut state = self.state.lock().unwrap();
                state.receiver = Some(receiver);

                Ok(())
            }
        }
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        if let Some(ref controller) = self.receiver_controller.lock().unwrap().take() {
            controller.shutdown();
        }
        *self.state.lock().unwrap() = State::default();
        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Scheduling(q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryViewMut::Latency(q) => {
                let state = self.state.lock().unwrap();
                let settings = self.settings.lock().unwrap();

                if let Some(latency) = state.current_latency {
                    let min = if matches!(
                        settings.timestamp_mode,
                        TimestampMode::ReceiveTimeTimecode | TimestampMode::ReceiveTimeTimestamp
                    ) {
                        latency
                    } else {
                        gst::ClockTime::ZERO
                    };

                    let max = settings.max_queue_length as u64 * latency;

                    debug!(
                        CAT,
                        obj = self.obj(),
                        "Returning latency min {} max {}",
                        min,
                        max
                    );
                    q.set(true, min, max);
                    true
                } else {
                    false
                }
            }
            _ => BaseSrcImplExt::parent_query(self, query),
        }
    }

    fn create(
        &self,
        _offset: u64,
        _buffer: Option<&mut gst::BufferRef>,
        _length: u32,
    ) -> Result<CreateSuccess, gst::FlowError> {
        let element = self.obj();
        let recv = {
            let mut state = self.state.lock().unwrap();
            match state.receiver.take() {
                Some(recv) => recv,
                None => {
                    error!(CAT, obj = element, "Have no receiver");
                    return Err(gst::FlowError::Error);
                }
            }
        };

        let res = recv.capture();

        let mut state = self.state.lock().unwrap();
        state.receiver = Some(recv);

        match res {
            ReceiverItem::Buffer(buffer) => {
                let buffer = match buffer {
                    Buffer::Audio(mut buffer, info) => {
                        if state.audio_info.as_ref() != Some(&info) {
                            let caps = info.to_caps().map_err(|_| {
                                gst::element_error!(
                                    element,
                                    gst::ResourceError::Settings,
                                    ["Invalid audio info received: {:?}", info]
                                );
                                gst::FlowError::NotNegotiated
                            })?;
                            state.audio_info = Some(info);
                            state.audio_caps = Some(caps);
                        }

                        {
                            let buffer = buffer.get_mut().unwrap();
                            ndisrcmeta::NdiSrcMeta::add(
                                buffer,
                                ndisrcmeta::StreamType::Audio,
                                state.audio_caps.as_ref().unwrap(),
                            );
                        }

                        buffer
                    }
                    Buffer::Video(mut buffer, info) => {
                        let mut latency_changed = false;

                        if state.video_info.as_ref() != Some(&info) {
                            let caps = info.to_caps().map_err(|_| {
                                gst::element_error!(
                                    element,
                                    gst::ResourceError::Settings,
                                    ["Invalid audio info received: {:?}", info]
                                );
                                gst::FlowError::NotNegotiated
                            })?;
                            state.video_info = Some(info);
                            state.video_caps = Some(caps);
                            latency_changed = state.current_latency != buffer.duration();
                            state.current_latency = buffer.duration();
                        }

                        {
                            let buffer = buffer.get_mut().unwrap();
                            ndisrcmeta::NdiSrcMeta::add(
                                buffer,
                                ndisrcmeta::StreamType::Video,
                                state.video_caps.as_ref().unwrap(),
                            );
                        }

                        drop(state);
                        if latency_changed {
                            let _ = element.post_message(
                                gst::message::Latency::builder().src(&*element).build(),
                            );
                        }

                        buffer
                    }
                };

                Ok(CreateSuccess::NewBuffer(buffer))
            }
            ReceiverItem::Timeout => Err(gst::FlowError::Eos),
            ReceiverItem::Flushing => Err(gst::FlowError::Flushing),
            ReceiverItem::Error(err) => Err(err),
        }
    }
}
