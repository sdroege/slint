// Copyright © SixtyFPS GmbH <info@slint-ui.com>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-commercial

use gst::prelude::*;
use gst_gl::prelude::*;

use std::sync::{Arc, Mutex};

slint::include_modules!();

struct Player<C: slint::ComponentHandle + 'static> {
    app: slint::Weak<C>,
    pipeline: gst::Pipeline,
    appsink: gst_app::AppSink,
    next_frame: Arc<Mutex<Option<(gst_video::VideoInfo, gst::Buffer)>>>,
    current_frame: Mutex<Option<gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>>>,
    gst_gl_context: Option<gst_gl::GLContext>,
}

impl<C: slint::ComponentHandle + 'static> Player<C> {
    fn new(app: slint::Weak<C>) -> Result<Self, anyhow::Error> {
        gst::init()?;

        let caps = gst::Caps::builder("video/x-raw")
            .features([gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY])
            .field("format", gst_video::VideoFormat::Rgba.to_str())
            .field("texture-target", "2D")
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build();

        let appsink = gst_app::AppSink::builder()
            .name("appsink")
            .enable_last_sample(false)
            .max_buffers(1u32)
            .caps(&caps)
            .build();

        let glsink = gst::ElementFactory::make("glsinkbin").property("sink", &appsink).build()?;

        let pipeline = gst::ElementFactory::make("playbin")
            .property(
                "uri",
                "https://gstreamer.freedesktop.org/data/media/sintel_trailer-480p.webm",
            )
            .property("video-sink", glsink)
            .build()?
            .downcast::<gst::Pipeline>()
            .unwrap();

        Ok(Self {
            app,
            pipeline,
            appsink,
            next_frame: Arc::new(Mutex::new(None)),
            current_frame: Mutex::new(None),
            gst_gl_context: None,
        })
    }

    fn setup_graphics(&mut self, graphics_api: &slint::GraphicsAPI) {
        let egl = match graphics_api {
            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
                glutin_egl_sys::egl::Egl::load_with(|symbol| {
                    get_proc_address(&std::ffi::CString::new(symbol).unwrap())
                })
            }
            _ => panic!("unsupported graphics API"),
        };

        let (gst_gl_context, gst_gl_display) = unsafe {
            let platform = gst_gl::GLPlatform::EGL;

            let egl_display = egl.GetCurrentDisplay();
            let display = gst_gl_egl::GLDisplayEGL::with_egl_display(egl_display as usize).unwrap();
            let native_context = egl.GetCurrentContext();

            (
                gst_gl::GLContext::new_wrapped(
                    &display,
                    native_context as _,
                    platform,
                    gst_gl::GLContext::current_gl_api(platform).0,
                )
                .expect("unable to create wrapped GL context"),
                display,
            )
        };

        gst_gl_context.activate(true).expect("could not activate GStreamer GL context");
        gst_gl_context.fill_info().expect("failed to fill GL info for wrapped context");

        self.gst_gl_context = Some(gst_gl_context.clone());

        let bus = self.pipeline.bus().unwrap();
        bus.set_sync_handler({
            let gst_gl_context = gst_gl_context.clone();
            move |_, msg| {
                match msg.view() {
                    gst::MessageView::NeedContext(ctx) => {
                        let ctx_type = ctx.context_type();
                        if ctx_type == *gst_gl::GL_DISPLAY_CONTEXT_TYPE {
                            if let Some(element) =
                                msg.src().and_then(|source| source.downcast_ref::<gst::Element>())
                            {
                                let gst_context = gst::Context::new(ctx_type, true);
                                gst_context.set_gl_display(&gst_gl_display);
                                element.set_context(&gst_context);
                            }
                        } else if ctx_type == "gst.gl.app_context" {
                            if let Some(element) =
                                msg.src().and_then(|source| source.downcast_ref::<gst::Element>())
                            {
                                let mut gst_context = gst::Context::new(ctx_type, true);
                                {
                                    let gst_context = gst_context.get_mut().unwrap();
                                    let structure = gst_context.structure_mut();
                                    structure.set("context", &gst_gl_context);
                                }
                                element.set_context(&gst_context);
                            }
                        }
                    }
                    _ => (),
                }

                gst::BusSyncReply::Drop
            }
        });

        let app_weak = self.app.clone();

        let current_sample_ref = self.next_frame.clone();

        self.appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;

                    let mut buffer = sample.buffer_owned().unwrap();
                    {
                        let context = match (buffer.n_memory() > 0)
                            .then(|| buffer.peek_memory(0))
                            .and_then(|m| m.downcast_memory_ref::<gst_gl::GLBaseMemory>())
                            .map(|m| m.context())
                        {
                            Some(context) => context.clone(),
                            None => {
                                eprintln!("Got non-GL memory");
                                return Err(gst::FlowError::Error);
                            }
                        };

                        if let Some(meta) = buffer.meta::<gst_gl::GLSyncMeta>() {
                            meta.set_sync_point(&context);
                        } else {
                            let buffer = buffer.make_mut();
                            let meta = gst_gl::GLSyncMeta::add(buffer, &context);
                            meta.set_sync_point(&context);
                        }
                    }

                    let Some(info) =
                        sample.caps().and_then(|caps| gst_video::VideoInfo::from_caps(caps).ok())
                    else {
                        eprintln!("Got invalid caps");
                        return Err(gst::FlowError::NotNegotiated);
                    };

                    let current_sample_ref = current_sample_ref.clone();
                    app_weak
                        .upgrade_in_event_loop(move |app| {
                            *current_sample_ref.lock().unwrap() = Some((info, buffer));

                            app.window().request_redraw();
                        })
                        .ok();

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        self.pipeline.set_state(gst::State::Playing).unwrap();
    }
}

impl<C: slint::ComponentHandle + 'static> Drop for Player<C> {
    fn drop(&mut self) {
        *self.next_frame.lock().unwrap() = None;
        *self.current_frame.lock().unwrap() = None;
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(ref gst_gl_context) = self.gst_gl_context {
            gst_gl_context.activate(false).expect("could not deactivate GStreamer GL context");
        }
    }
}

pub fn main() -> Result<(), anyhow::Error> {
    let main_window = MainWindow::new()?;

    let mut player = Player::new(main_window.as_weak())?;

    let mw_weak = main_window.as_weak();

    if let Err(error) =
        main_window.window().set_rendering_notifier(move |state, graphics_api| match state {
            slint::RenderingState::RenderingSetup => {
                player.setup_graphics(graphics_api);
            }
            slint::RenderingState::RenderingTeardown => {
                *player.next_frame.lock().unwrap() = None;
                *player.current_frame.lock().unwrap() = None;
                let _ = player.pipeline.set_state(gst::State::Null);
                if let Some(ref gst_gl_context) = player.gst_gl_context {
                    gst_gl_context
                        .activate(false)
                        .expect("could not deactivate GStreamer GL context");
                }
            }
            slint::RenderingState::BeforeRendering => {
                if let Some((info, buffer)) = player.next_frame.lock().unwrap().take() {
                    let sync_meta = buffer.meta::<gst_gl::GLSyncMeta>().unwrap();
                    sync_meta.wait(player.gst_gl_context.as_ref().unwrap());

                    if let Ok(frame) = gst_gl::GLVideoFrame::from_buffer_readable(buffer, &info) {
                        *player.current_frame.lock().unwrap() = Some(frame);
                    }
                }

                if let Some(frame) = player.current_frame.lock().unwrap().as_ref() {
                    if let Some(texture) =
                        frame.texture_id(0).ok().and_then(|id| id.try_into().ok())
                    {
                        mw_weak.unwrap().set_texture(unsafe {
                            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                                texture,
                                [frame.width(), frame.height()].into(),
                            )
                            .build()
                        });
                    }
                }
            }
            _ => {} // Nothing to do
        })
    {
        match error {
            slint::SetRenderingNotifierError::Unsupported => eprintln!("This example requires the use of the GL backend. Please run with the environment variable SLINT_BACKEND=GL set."),
            _ => unreachable!()
        }
        std::process::exit(1);
    };

    main_window.run()?;
    Ok(())
}
