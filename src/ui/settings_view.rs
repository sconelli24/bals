use std::path::PathBuf;

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::config::{
    AudioCaptureMode, CameraCorner, CameraSize, CaptureMode, EncoderPreference, FrameRate,
    QualityPreset, ReplayDuration, Resolution, UiTheme, VideoConfig,
};
use crate::ui::theme::RetroPalette;
use crate::ui::widgets::{
    SectionIcon, draw_camera_corner_button, draw_led_vu_meter, retro_card,
    retro_card_custom_header, retro_checkbox, retro_pill, retro_save_button,
};
use crate::ui::{BalsUiApp, UpdateUiState, open_in_shell};
use crate::updater::{CURRENT_VERSION, ReleaseInfo};

/// Pestaña 1 — Dashboard estilo Consola Retro OS en grilla modular 2×2 sin scroll.
pub fn render_settings_tab(app: &mut BalsUiApp, ui: &mut egui::Ui, p: &RetroPalette) {
    // Fila superior: VIDEO (izquierda) + AUDIO (derecha)
    ui.columns(2, |cols| {
        cols[0].vertical(|ui| {
            render_video_card(app, ui, p);
        });
        cols[1].vertical(|ui| {
            render_audio_card(app, ui, p);
        });
    });

    ui.add_space(6.0);

    // Fila inferior: CAPTURA Y ATAJO (izquierda) + CÁMARA y SISTEMA Y TEMA (derecha)
    ui.columns(2, |cols| {
        cols[0].vertical(|ui| {
            render_capture_hotkey_card(app, ui, p);
        });
        cols[1].vertical(|ui| {
            render_camera_card(app, ui, p);
            ui.add_space(6.0);
            render_system_card(app, ui, p);
        });
    });

    if app.show_exe_picker {
        render_exe_picker_modal(app, ui.ctx(), p);
    }
}

fn render_video_card(app: &mut BalsUiApp, ui: &mut egui::Ui, p: &RetroPalette) {
    let hw_badge = app
        .detected_encoders
        .hw_short
        .as_deref()
        .map(|s| format!("H.264 · {s}"))
        .unwrap_or_else(|| "H.264 SW".to_string());

    retro_card(
        ui,
        p,
        Some(SectionIcon::Video),
        "VIDEO",
        Some(&hw_badge),
        |ui| {
            let row_w = ui.available_width();
            let sp = ui.spacing().item_spacing.x;

            // Resolución (3 píldoras)
            ui.label(egui::RichText::new("RESOLUCIÓN").size(10.0).color(p.crt_muted));
            let w3 = ((row_w - sp * 2.0) / 3.0).max(70.0);
            ui.horizontal(|ui| {
                if retro_pill(ui, p, app.cfg.resolution == Resolution::P720, "720p", vec2(w3, 22.0)).clicked() {
                    app.cfg.resolution = Resolution::P720;
                }
                if retro_pill(ui, p, app.cfg.resolution == Resolution::P1080, "1080p", vec2(w3, 22.0)).clicked() {
                    app.cfg.resolution = Resolution::P1080;
                }
                if retro_pill(ui, p, app.cfg.resolution == Resolution::P1440, "2K", vec2(w3, 22.0)).clicked() {
                    app.cfg.resolution = Resolution::P1440;
                }
            });

            ui.add_space(2.0);

            // FPS (2 píldoras) | Calidad QVBR (3 píldoras) con separador visual claro
            let sep_w = 9.0;
            let usable = row_w - sp * 4.0 - sep_w;
            let w_fps_btn = ((usable * 0.36) / 2.0).max(52.0);
            let w_qual = ((usable - w_fps_btn * 2.0) / 3.0).max(58.0);
            let fps_group_w = w_fps_btn * 2.0 + sp;

            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    vec2(fps_group_w, 13.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new("FPS").size(10.2).color(p.crt_muted));
                    },
                );
                ui.add_space(sep_w);
                ui.label(
                    egui::RichText::new("CALIDAD (QVBR)")
                        .size(10.2)
                        .color(p.crt_muted),
                );
            });

            ui.horizontal(|ui| {
                if retro_pill(ui, p, app.cfg.fps == FrameRate::Fps30, "30 FPS", vec2(w_fps_btn, 22.0)).clicked() {
                    app.cfg.fps = FrameRate::Fps30;
                }
                if retro_pill(ui, p, app.cfg.fps == FrameRate::Fps60, "60 FPS", vec2(w_fps_btn, 22.0)).clicked() {
                    app.cfg.fps = FrameRate::Fps60;
                }

                let (sep_rect, _) = ui.allocate_exact_size(vec2(sep_w, 22.0), egui::Sense::hover());
                ui.painter().line_segment(
                    [
                        pos2(sep_rect.center().x, sep_rect.top() + 3.0),
                        pos2(sep_rect.center().x, sep_rect.bottom() - 3.0),
                    ],
                    Stroke::new(1.0, p.crt_widget_hover),
                );

                if retro_pill(
                    ui,
                    p,
                    app.cfg.quality == QualityPreset::Compact,
                    "AHORRO",
                    vec2(w_qual, 22.0),
                )
                .on_hover_text("~45% menos peso y RAM")
                .clicked()
                {
                    app.cfg.quality = QualityPreset::Compact;
                }
                if retro_pill(
                    ui,
                    p,
                    app.cfg.quality == QualityPreset::Balanced,
                    "NORMAL",
                    vec2(w_qual, 22.0),
                )
                .clicked()
                {
                    app.cfg.quality = QualityPreset::Balanced;
                }
                if retro_pill(
                    ui,
                    p,
                    app.cfg.quality == QualityPreset::High,
                    "ALTA",
                    vec2(w_qual, 22.0),
                )
                .clicked()
                {
                    app.cfg.quality = QualityPreset::High;
                }
            });

            ui.add_space(2.0);

            // Encoder H.264 (GPU HW / CPU SW — preseleccionado automáticamente al iniciar)
            ui.label(egui::RichText::new("ENCODER H.264").size(10.0).color(p.crt_muted));
            let w_enc_btn = ((row_w - sp) / 2.0).max(100.0);

            let gpu_pill_label = app
                .detected_encoders
                .hw_short
                .as_deref()
                .map(|s| format!("GPU ({s})"))
                .unwrap_or_else(|| "GPU (NO DISP.)".to_string());
            let hw_hover = app
                .detected_encoders
                .hw_name
                .as_deref()
                .map(|n| match app.detected_encoders.adapter_name.as_deref() {
                    Some(adapter) => format!(
                        "GPU activa: {adapter}\nEncoder H.264: {n} (con fallback seguro a CPU)"
                    ),
                    None => format!(
                        "Codificación H.264 por hardware en tu gráfica: {n} (con fallback seguro a CPU)"
                    ),
                })
                .unwrap_or_else(|| "No se detectó encoder H.264 por hardware en esta PC".to_string());
            let sw_hover = app
                .detected_encoders
                .sw_name
                .as_deref()
                .map(|n| format!("Codificación H.264 por CPU (software): {n}"))
                .unwrap_or_else(|| "Codificación H.264 por CPU (Microsoft H.264 MFT)".to_string());

            ui.horizontal(|ui| {
                ui.add_enabled_ui(app.detected_encoders.hw_name.is_some(), |ui| {
                    if retro_pill(
                        ui,
                        p,
                        app.cfg.encoder_pref == EncoderPreference::HardwareGpu,
                        &gpu_pill_label,
                        vec2(w_enc_btn, 22.0),
                    )
                    .on_hover_text(&hw_hover)
                    .clicked()
                    {
                        app.cfg.encoder_pref = EncoderPreference::HardwareGpu;
                    }
                });
                if retro_pill(
                    ui,
                    p,
                    app.cfg.encoder_pref == EncoderPreference::SoftwareCpu,
                    "CPU (SW)",
                    vec2(w_enc_btn, 22.0),
                )
                .on_hover_text(&sw_hover)
                .clicked()
                {
                    app.cfg.encoder_pref = EncoderPreference::SoftwareCpu;
                }
            });

            ui.add_space(2.0);

            // Duración del Replay Buffer (4 píldoras)
            ui.label(egui::RichText::new("REPLAY BUFFER EN RAM").size(10.0).color(p.crt_muted));
            let w4 = ((row_w - sp * 3.0) / 4.0).max(52.0);
            ui.horizontal(|ui| {
                for (dur, lbl) in [
                    (ReplayDuration::Sec15, "15s"),
                    (ReplayDuration::Sec30, "30s"),
                    (ReplayDuration::Sec60, "60s"),
                    (ReplayDuration::Sec120, "120s"),
                ] {
                    if retro_pill(ui, p, app.cfg.replay_duration == dur, lbl, vec2(w4, 22.0)).clicked() {
                        app.cfg.replay_duration = dur;
                    }
                }
            });

            ui.add_space(5.0);
            let bitrate_bps = VideoConfig::recommended_bitrate_for(
                app.cfg.resolution,
                app.cfg.fps,
                app.cfg.quality,
            );
            let est_ram_mb = (bitrate_bps as f64 / 8.0) * app.cfg.replay_duration.as_secs() as f64
                / (1024.0 * 1024.0);
            ui.label(
                egui::RichText::new(format!(
                    "{:.1} Mbps (H.264) · ~{:.1} MB RAM · 0 MB/s DISCO",
                    bitrate_bps as f64 / 1_000_000.0,
                    est_ram_mb
                ))
                .size(10.8)
                .color(p.accent),
            );
        },
    );
}

fn render_audio_card(app: &mut BalsUiApp, ui: &mut egui::Ui, p: &RetroPalette) {
    retro_card(
        ui,
        p,
        Some(SectionIcon::Audio),
        "AUDIO",
        Some("48kHz AAC"),
        |ui| {
            let row_w = ui.available_width();
            let sp = ui.spacing().item_spacing.x;
            let w2 = ((row_w - sp) / 2.0).max(95.0);

            ui.label(egui::RichText::new("FUENTE DE CAPTURA").size(10.0).color(p.crt_muted));
            ui.horizontal(|ui| {
                if retro_pill(
                    ui,
                    p,
                    app.cfg.audio_capture_mode == AudioCaptureMode::FullDesktop,
                    "ESCRITORIO",
                    vec2(w2, 24.0),
                )
                .on_hover_text("Juego + Discord + música del sistema")
                .clicked()
                {
                    app.cfg.audio_capture_mode = AudioCaptureMode::FullDesktop;
                }
                if retro_pill(
                    ui,
                    p,
                    app.cfg.audio_capture_mode == AudioCaptureMode::GameOnly,
                    "SOLO JUEGO",
                    vec2(w2, 24.0),
                )
                .on_hover_text("Excluye Discord y música (ProcessLoopback)")
                .clicked()
                {
                    app.cfg.audio_capture_mode = AudioCaptureMode::GameOnly;
                }
            });

            ui.add_space(7.0);

            // Volumen Juego / Escritorio + VU LED segmentado (etiquetas alineadas a ancho fijo)
            ui.horizontal(|ui| {
                ui.add_sized(
                    vec2(78.0, 18.0),
                    egui::Label::new(egui::RichText::new("JUEGO / PC").size(11.5).color(p.crt_text)),
                );
                let mut pct = (app.cfg.game_volume * 100.0).round();
                if ui
                    .add(
                        egui::Slider::new(&mut pct, 0.0..=200.0)
                            .suffix("%")
                            .show_value(true),
                    )
                    .changed()
                {
                    app.cfg.game_volume = pct / 100.0;
                }
            });
            draw_led_vu_meter(
                ui,
                p,
                app.desktop_vu_display,
                24,
                14.0,
                "Nivel juego / escritorio",
            );

            ui.add_space(8.0);

            retro_checkbox(ui, p, &mut app.cfg.mic_enabled, "MEZCLAR MICRÓFONO");

            ui.add_enabled_ui(app.cfg.mic_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        vec2(78.0, 18.0),
                        egui::Label::new(egui::RichText::new("MICRÓFONO").size(11.5).color(p.crt_text)),
                    );
                    let mut pct = (app.cfg.mic_volume * 100.0).round();
                    if ui
                        .add(
                            egui::Slider::new(&mut pct, 0.0..=200.0)
                                .suffix("%")
                                .show_value(true),
                        )
                        .changed()
                    {
                        app.cfg.mic_volume = pct / 100.0;
                    }
                });
                draw_led_vu_meter(
                    ui,
                    p,
                    if app.cfg.mic_enabled {
                        app.mic_vu_display
                    } else {
                        0.0
                    },
                    24,
                    14.0,
                    "Nivel micrófono",
                );
            });

            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Ajustar volúmenes al guardar NO reinicia el Replay Buffer.")
                    .size(10.5)
                    .color(p.crt_muted),
            );
        },
    );
}

fn render_capture_hotkey_card(app: &mut BalsUiApp, ui: &mut egui::Ui, p: &RetroPalette) {
    let mode_badge = match app.cfg.capture_mode {
        CaptureMode::AutoDetectGames => "0% GPU IDLE",
        CaptureMode::ManualMonitor => "MONITOR",
    };

    retro_card(
        ui,
        p,
        Some(SectionIcon::Capture),
        "CAPTURA Y ATAJO",
        Some(mode_badge),
        |ui| {
            let row_w = ui.available_width();
            let sp = ui.spacing().item_spacing.x;
            let w_mode = ((row_w - sp) / 2.0).max(110.0);

            ui.label(egui::RichText::new("MODO DE CAPTURA").size(10.0).color(p.crt_muted));
            ui.horizontal(|ui| {
                if retro_pill(
                    ui,
                    p,
                    app.cfg.capture_mode == CaptureMode::AutoDetectGames,
                    "JUEGOS",
                    vec2(w_mode, 24.0),
                )
                .on_hover_text("Detecta juegos automáticamente y libera 100% GPU en el escritorio")
                .clicked()
                {
                    app.cfg.capture_mode = CaptureMode::AutoDetectGames;
                }
                if retro_pill(
                    ui,
                    p,
                    app.cfg.capture_mode == CaptureMode::ManualMonitor,
                    "MONITOR",
                    vec2(w_mode, 24.0),
                )
                .on_hover_text("Graba continuamente toda la pantalla seleccionada")
                .clicked()
                {
                    app.cfg.capture_mode = CaptureMode::ManualMonitor;
                    if app.monitors.is_empty() {
                        app.refresh_monitors();
                    }
                }
            });

            ui.add_space(5.0);

            // Sección contextual según el modo elegido (Juegos en ventana vs Selección de Monitor)
            match app.cfg.capture_mode {
                CaptureMode::AutoDetectGames => {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("JUEGOS EN VENTANA (.EXE)")
                                .size(10.0)
                                .color(p.crt_muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if retro_pill(ui, p, false, "+ AGREGAR .EXE", vec2(114.0, 20.0))
                                .on_hover_text("Detecta programas activos para agregarlos sin escribir el nombre")
                                .clicked()
                            {
                                app.open_exe_picker();
                            }
                        });
                    });

                    if app.cfg.extra_game_exes.is_empty() {
                        ui.label(
                            egui::RichText::new(
                                "Pantalla completa es automática. Agrega un .exe si juegas en ventana.",
                            )
                            .size(10.5)
                            .color(p.crt_muted),
                        );
                    } else {
                        let mut remove_idx: Option<usize> = None;
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = vec2(5.0, 4.0);
                            for (idx, exe) in app.cfg.extra_game_exes.iter().enumerate() {
                                let label = format!("{exe}  ×");
                                let chip_w = (label.chars().count() as f32 * 6.8 + 16.0).max(70.0);
                                if retro_pill(ui, p, false, &label, vec2(chip_w, 21.0))
                                    .on_hover_text("Clic para quitar de la lista")
                                    .clicked()
                                {
                                    remove_idx = Some(idx);
                                }
                            }
                        });
                        if let Some(idx) = remove_idx {
                            app.cfg.extra_game_exes.remove(idx);
                        }
                    }
                }
                CaptureMode::ManualMonitor => {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("PANTALLA A GRABAR")
                                .size(10.0)
                                .color(p.crt_muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if retro_pill(ui, p, false, "RECARGAR", vec2(74.0, 20.0))
                                .on_hover_text("Volver a detectar monitores conectados")
                                .clicked()
                            {
                                app.refresh_monitors();
                            }
                        });
                    });

                    if app.monitors.is_empty() {
                        ui.label(
                            egui::RichText::new("Usando monitor principal del sistema.")
                                .size(11.0)
                                .color(p.crt_muted),
                        );
                    } else {
                        let selected_dev = app.cfg.selected_monitor.as_deref();
                        let per_row = app.monitors.len().clamp(1, 2);
                        let pill_w =
                            ((row_w - sp * (per_row as f32 - 1.0)) / per_row as f32).max(120.0);
                        let mut chosen_dev: Option<String> = None;

                        for chunk in app.monitors.chunks(2) {
                            ui.horizontal(|ui| {
                                for m in chunk {
                                    let is_sel = match selected_dev {
                                        Some(dev) => m.device_name.eq_ignore_ascii_case(dev),
                                        None => m.is_primary || m.index == 1,
                                    };
                                    let hover = if m.is_primary {
                                        format!("Monitor principal ({})", m.device_name)
                                    } else {
                                        format!("Monitor secundario ({})", m.device_name)
                                    };
                                    if retro_pill(ui, p, is_sel, &m.ui_label(), vec2(pill_w, 22.0))
                                        .on_hover_text(hover)
                                        .clicked()
                                    {
                                        chosen_dev = Some(m.device_name.clone());
                                    }
                                }
                            });
                        }

                        if let Some(dev) = chosen_dev {
                            app.cfg.selected_monitor = Some(dev);
                        }
                    }
                }
            }

            ui.add_space(6.0);
            ui.label(egui::RichText::new("TECLA PARA GUARDAR CLIP").size(10.0).color(p.crt_muted));

            // Keycap limpia (sin corchetes `[ ]`) + botón único ASIGNAR TECLA
            ui.horizontal(|ui| {
                let key_display = if app.listening_hotkey {
                    "...".to_string()
                } else {
                    crate::hotkey::display_hotkey(&app.cfg.save_clip_hotkey)
                };
                let (krect, _) = ui.allocate_exact_size(vec2(96.0, 28.0), Sense::hover());
                let painter = ui.painter();
                let kborder = if app.listening_hotkey {
                    p.amber
                } else {
                    p.crt_panel_border
                };
                painter.rect_filled(krect, 3.0, p.crt_recess);
                painter.rect_stroke(krect, 3.0, Stroke::new(1.5, kborder), StrokeKind::Inside);
                painter.text(
                    krect.center(),
                    Align2::CENTER_CENTER,
                    &key_display,
                    FontId::monospace(13.0),
                    if app.listening_hotkey {
                        p.amber
                    } else {
                        p.accent_bright
                    },
                );

                let bind_lbl = if app.listening_hotkey {
                    "PRESIONA UNA TECLA... (ESC)"
                } else {
                    "ASIGNAR TECLA"
                };
                let btn_w = ui.available_width().max(130.0);
                if retro_pill(ui, p, app.listening_hotkey, bind_lbl, vec2(btn_w, 28.0)).clicked() {
                    app.set_listening_hotkey(!app.listening_hotkey);
                }
            });

            if let Some(err) = &app.hotkey_error {
                ui.label(egui::RichText::new(err).size(11.0).color(p.led_red));
            }

            ui.add_space(6.0);

            // Checkboxes en 2 columnas limpias (6 slots simétricos)
            ui.columns(2, |c| {
                retro_checkbox(&mut c[0], p, &mut app.cfg.capture_cursor, "Capturar cursor");
                retro_checkbox(&mut c[0], p, &mut app.cfg.sound_feedback, "Sonido al guardar");
                retro_checkbox(&mut c[0], p, &mut app.cfg.show_notifications, "Notificaciones");
                retro_checkbox(&mut c[1], p, &mut app.cfg.stretch_aspect_ratio, "Estirar 4:3")
                    .on_hover_text(
                        "Estira juegos en 4:3 / 5:4 / 16:10 a 16:9 completo tal como los ves al jugar (sin barras negras)",
                    );
                retro_checkbox(&mut c[1], p, &mut app.cfg.auto_update, "Auto-actualizar");
                retro_checkbox(
                    &mut c[1],
                    p,
                    &mut app.start_with_windows,
                    "Iniciar con Windows",
                );
            });
        },
    );
}

fn render_camera_card(app: &mut BalsUiApp, ui: &mut egui::Ui, p: &RetroPalette) {
    let active_cam_idx = if app.cameras.is_empty() {
        None
    } else {
        Some(
            app.cfg
                .selected_camera
                .as_deref()
                .and_then(|sel| {
                    app.cameras
                        .iter()
                        .position(|c| c.symbolic_link.eq_ignore_ascii_case(sel))
                })
                .unwrap_or(0),
        )
    };

    let cam_enabled = app.cfg.camera_enabled;
    let cameras_loading = app.cameras_loading;
    let active_cam_presets = active_cam_idx.map(|i| app.cameras[i].supported_presets.clone());
    let resolved_preset =
        active_cam_idx.and_then(|i| app.cameras[i].resolve_preset(app.cfg.camera_preset));
    let mut cycle_preset = false;

    retro_card_custom_header(
        ui,
        p,
        Some(SectionIcon::Camera),
        "CÁMARA (WEBCAM)",
        |ui| {
            if !cam_enabled {
                ui.label(egui::RichText::new("OFF · 0% CPU").size(11.0).color(p.amber));
            } else if cameras_loading && active_cam_idx.is_none() {
                ui.label(egui::RichText::new("BUSCANDO...").size(10.5).color(p.amber));
            } else if active_cam_idx.is_some() {
                let presets = active_cam_presets.as_deref().unwrap_or(&[]);
                let has_multiple = presets.len() > 1;
                let preset_txt = match (resolved_preset, has_multiple) {
                    (Some(pr), true) => format!("{} ▾", pr.label()),
                    (Some(pr), false) => pr.label(),
                    (None, _) => "AUTO".to_string(),
                };
                let hover = if has_multiple {
                    let list = presets
                        .iter()
                        .map(|pr| pr.label())
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    format!("Clic para cambiar resolución y FPS de la cámara\nDisponibles: {list}")
                } else if let Some(only) = resolved_preset {
                    format!("Único formato disponible en esta cámara: {}", only.label())
                } else {
                    "Formato automático detectado por la cámara".to_string()
                };

                let pill_w = (preset_txt.chars().count() as f32 * 7.0 + 16.0).max(72.0);
                if retro_pill(ui, p, has_multiple, &preset_txt, vec2(pill_w, 19.0))
                    .on_hover_text(hover)
                    .clicked()
                    && has_multiple
                {
                    cycle_preset = true;
                }
            } else {
                ui.label(egui::RichText::new("SIN CÁMARA").size(11.0).color(p.amber));
            }
        },
        |ui| {
            let sp = ui.spacing().item_spacing.x;

            // Fila 1: Toggle activar + selector de dispositivo + botón buscar USB
            ui.horizontal(|ui| {
                retro_checkbox(ui, p, &mut app.cfg.camera_enabled, "ACTIVAR")
                    .on_hover_text("Incluye la webcam en el clip (se aplica en vivo sin perder el buffer)");

                ui.add_space(4.0);

                let act_w = 58.0;
                let dev_w = (ui.available_width() - act_w - sp).max(100.0);

                if let Some(current_idx) = active_cam_idx {
                    let current = &app.cameras[current_idx];
                    let max_chars = ((dev_w - 18.0) / 6.5).max(10.0) as usize;
                    let short_name = truncate_label(&current.friendly_name, max_chars);
                    let dev_label = if app.cameras.len() > 1 {
                        format!("{short_name} ({}/{})", current_idx + 1, app.cameras.len())
                    } else {
                        short_name
                    };
                    let hover = if app.cameras.len() > 1 {
                        format!(
                            "Cámara activa: {}\nClic para cambiar a la siguiente cámara detectada",
                            current.friendly_name
                        )
                    } else {
                        format!("Cámara detectada: {}", current.friendly_name)
                    };

                    if retro_pill(ui, p, app.cfg.camera_enabled, &dev_label, vec2(dev_w, 21.0))
                        .on_hover_text(hover)
                        .clicked()
                        && app.cameras.len() > 1
                    {
                        let next = (current_idx + 1) % app.cameras.len();
                        app.cfg.selected_camera = Some(app.cameras[next].symbolic_link.clone());
                        app.cfg.camera_preset =
                            app.cameras[next].resolve_preset(app.cfg.camera_preset);
                    }
                } else {
                    let status_lbl = if app.cameras_loading {
                        "Buscando cámaras..."
                    } else {
                        "Sin cámaras detectadas"
                    };
                    ui.add_sized(
                        vec2(dev_w, 20.0),
                        egui::Label::new(
                            egui::RichText::new(status_lbl)
                                .size(10.5)
                                .color(p.crt_muted),
                        ),
                    );
                }

                ui.add_enabled_ui(!app.cameras_loading, |ui| {
                    let btn_lbl = if app.cameras_loading { "..." } else { "BUSCAR" };
                    if retro_pill(ui, p, false, btn_lbl, vec2(act_w, 21.0))
                        .on_hover_text("Volver a detectar cámaras USB conectadas")
                        .clicked()
                    {
                        app.refresh_cameras();
                    }
                });
            });

            ui.add_space(4.0);

            // Fila 2: selector visual de 4 esquinas + 3 tamaños (S/M/L) + Espejo
            ui.add_enabled_ui(app.cfg.camera_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.5;
                    ui.label(egui::RichText::new("POS:").size(10.0).color(p.crt_muted));
                    for corner in CameraCorner::ALL {
                        if draw_camera_corner_button(
                            ui,
                            p,
                            corner,
                            app.cfg.camera_corner == corner,
                        )
                        .on_hover_text(format!("Esquina: {}", corner.description()))
                        .clicked()
                        {
                            app.cfg.camera_corner = corner;
                        }
                    }

                    ui.add_space(5.0);
                    ui.label(egui::RichText::new("TAM:").size(10.0).color(p.crt_muted));
                    for size in CameraSize::ALL {
                        if retro_pill(
                            ui,
                            p,
                            app.cfg.camera_size == size,
                            size.short_label(),
                            vec2(24.0, 22.0),
                        )
                        .on_hover_text(format!("Tamaño: {}", size.description()))
                        .clicked()
                        {
                            app.cfg.camera_size = size;
                        }
                    }

                    ui.add_space(5.0);

                    retro_checkbox(ui, p, &mut app.cfg.camera_mirror, "Espejo")
                        .on_hover_text("Invierte horizontalmente la imagen de la webcam");
                });
            });
        },
    );

    if cycle_preset {
        if let Some(idx) = active_cam_idx {
            app.cfg.camera_preset = app.cameras[idx].next_preset(resolved_preset);
        }
    }
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars || max_chars < 4 {
        return s.to_string();
    }
    let head: String = chars[..max_chars - 1].iter().collect();
    format!("{head}…")
}

fn render_system_card(app: &mut BalsUiApp, ui: &mut egui::Ui, p: &RetroPalette) {
    enum UpdaterSnapshot {
        Idle,
        Checking,
        UpToDate,
        Available(ReleaseInfo),
        Downloading,
        Error(String),
    }

    let upd_snap = app
        .update_state
        .lock()
        .map(|st| match &*st {
            UpdateUiState::Idle => UpdaterSnapshot::Idle,
            UpdateUiState::Checking => UpdaterSnapshot::Checking,
            UpdateUiState::UpToDate => UpdaterSnapshot::UpToDate,
            UpdateUiState::Available(info) => UpdaterSnapshot::Available(info.clone()),
            UpdateUiState::Downloading => UpdaterSnapshot::Downloading,
            UpdateUiState::Error(e) => UpdaterSnapshot::Error(e.clone()),
        })
        .unwrap_or(UpdaterSnapshot::Idle);

    if matches!(
        upd_snap,
        UpdaterSnapshot::Checking | UpdaterSnapshot::Downloading
    ) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(120));
    }

    let (badge_txt, badge_col, badge_hover) = match &upd_snap {
        UpdaterSnapshot::Idle => (format!("v{CURRENT_VERSION}"), p.amber, None),
        UpdaterSnapshot::Checking => (
            format!("v{CURRENT_VERSION} · BUSCANDO..."),
            p.amber,
            Some("Consultando GitHub Releases..."),
        ),
        UpdaterSnapshot::UpToDate => (
            format!("v{CURRENT_VERSION} · AL DÍA"),
            p.accent,
            Some("Ya tienes la última versión disponible"),
        ),
        UpdaterSnapshot::Available(info) => (
            format!("v{CURRENT_VERSION} -> v{}", info.version),
            p.accent_bright,
            Some("Nueva versión lista para instalar"),
        ),
        UpdaterSnapshot::Downloading => (
            format!("v{CURRENT_VERSION} · BAJANDO..."),
            p.amber,
            Some("Descargando actualización..."),
        ),
        UpdaterSnapshot::Error(e) => (
            format!("v{CURRENT_VERSION} · ERR"),
            p.led_red,
            Some(e.as_str()),
        ),
    };

    let dirty = app.has_unsaved_changes();
    let mut do_save = false;
    let mut do_check_updates = false;
    let mut pending_install: Option<ReleaseInfo> = None;

    retro_card_custom_header(
        ui,
        p,
        Some(SectionIcon::System),
        "SISTEMA Y TEMA",
        |ui| {
            let lbl = ui.label(egui::RichText::new(&badge_txt).size(10.8).color(badge_col));
            if let Some(hover) = badge_hover {
                lbl.on_hover_text(hover);
            }
        },
        |ui| {
            let sp = ui.spacing().item_spacing.x;

            // Carpeta de clips
            ui.label(egui::RichText::new("CARPETA DE CLIPS").size(10.0).color(p.crt_muted));
            ui.horizontal(|ui| {
                let mut dir_str = app.cfg.clips_dir.to_string_lossy().into_owned();
                let input_w = (ui.available_width() - 66.0 - sp - 6.0).max(120.0);
                if ui
                    .add(egui::TextEdit::singleline(&mut dir_str).desired_width(input_w))
                    .changed()
                {
                    app.cfg.clips_dir = PathBuf::from(dir_str);
                }
                if retro_pill(ui, p, false, "ABRIR", vec2(66.0, 22.0)).clicked() {
                    let _ = std::fs::create_dir_all(&app.cfg.clips_dir);
                    open_in_shell(&app.cfg.clips_dir);
                }
            });

            ui.add_space(6.0);

            // Selector de Tema Retro + toggle Scanlines CRT
            ui.label(egui::RichText::new("APARIENCIA").size(10.0).color(p.crt_muted));
            ui.horizontal(|ui| {
                let crt_w = 56.0;
                let avail = (ui.available_width() - crt_w - sp * 4.0).max(160.0);
                let tw = (avail / 4.0).max(42.0);
                for theme in UiTheme::ALL {
                    if retro_pill(
                        ui,
                        p,
                        app.cfg.ui_theme == theme,
                        theme.short_label(),
                        vec2(tw, 22.0),
                    )
                    .clicked()
                    {
                        app.cfg.ui_theme = theme;
                    }
                }
                retro_checkbox(ui, p, &mut app.cfg.crt_scanlines, "CRT");
            });

            ui.add_space(6.0);

            // Botón principal GUARDAR (vectorial, altura fija) + Auto-updater integrado
            let avail_w = ui.available_width();
            ui.horizontal(|ui| {
                let save_w = ((avail_w - sp) * 0.60).max(165.0);
                let save_hover = if dirty {
                    "Tienes cambios sin aplicar — clic o Ctrl+S para guardar"
                } else {
                    "Todos los ajustes están guardados y aplicados (Ctrl+S)"
                };
                if retro_save_button(ui, p, dirty, vec2(save_w, 30.0))
                    .on_hover_text(save_hover)
                    .clicked()
                {
                    do_save = true;
                }

                let upd_w = (avail_w - save_w - sp).max(96.0);
                match &upd_snap {
                    UpdaterSnapshot::Available(info) => {
                        if retro_pill(
                            ui,
                            p,
                            true,
                            &format!("INSTALAR v{}", info.version),
                            vec2(upd_w, 30.0),
                        )
                        .on_hover_text("Descargar e instalar la nueva versión")
                        .clicked()
                        {
                            pending_install = Some(info.clone());
                        }
                    }
                    UpdaterSnapshot::Checking | UpdaterSnapshot::Downloading => {
                        let lbl = if matches!(upd_snap, UpdaterSnapshot::Checking) {
                            "BUSCANDO..."
                        } else {
                            "BAJANDO..."
                        };
                        ui.add_enabled_ui(false, |ui| {
                            let _ = retro_pill(ui, p, false, lbl, vec2(upd_w, 30.0));
                        });
                    }
                    _ => {
                        if retro_pill(ui, p, false, "UPDATES", vec2(upd_w, 30.0))
                            .on_hover_text("Buscar nueva versión en GitHub Releases")
                            .clicked()
                        {
                            do_check_updates = true;
                        }
                    }
                }
            });
        },
    );

    if do_save {
        app.save_config();
    }
    if do_check_updates {
        app.check_for_updates();
    }
    if let Some(info) = pending_install {
        app.install_update(info);
    }
}

/// Ventana modal estilo vibranceGUI para elegir procesos `.exe` activos en el escritorio.
fn render_exe_picker_modal(app: &mut BalsUiApp, ctx: &egui::Context, p: &RetroPalette) {
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.show_exe_picker = false;
        return;
    }

    let modal_frame = egui::Frame::new()
        .fill(p.crt_panel_bg)
        .stroke(Stroke::new(2.0, p.accent))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(egui::Margin::same(14));

    let mut keep_open = app.show_exe_picker;
    let mut clicked_exe: Option<String> = None;

    egui::Window::new("SELECCIONAR PROGRAMA ACTIVO (.EXE)")
        .open(&mut keep_open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .fixed_size(vec2(490.0, 360.0))
        .frame(modal_frame)
        .show(ctx, |ui| {
            let sp = ui.spacing().item_spacing.x;

            // Barra superior: filtro + botón actualizar
            ui.horizontal(|ui| {
                let filter_w = (ui.available_width() - 96.0 - sp).max(160.0);
                ui.add(
                    egui::TextEdit::singleline(&mut app.exe_picker_filter)
                        .hint_text("Filtrar por .exe o título de ventana...")
                        .desired_width(filter_w),
                );
                if retro_pill(ui, p, false, "ACTUALIZAR", vec2(96.0, 24.0))
                    .on_hover_text("Volver a escanear las ventanas abiertas")
                    .clicked()
                {
                    app.refresh_running_apps();
                }
            });

            ui.add_space(6.0);

            let needle = app.exe_picker_filter.trim().to_ascii_lowercase();
            let filtered: Vec<_> = app
                .running_apps
                .iter()
                .filter(|item| {
                    needle.is_empty()
                        || item.exe_name.to_ascii_lowercase().contains(&needle)
                        || item.window_title.to_ascii_lowercase().contains(&needle)
                })
                .cloned()
                .collect();

            if filtered.is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(
                            "No se detectaron programas abiertos con ese filtro.\nAbre tu juego y pulsa ACTUALIZAR.",
                        )
                        .size(11.5)
                        .color(p.crt_muted),
                    );
                });
            } else {
                egui::ScrollArea::vertical()
                    .max_height(248.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for item in &filtered {
                            let is_added = app
                                .cfg
                                .extra_game_exes
                                .iter()
                                .any(|g| g.eq_ignore_ascii_case(&item.exe_name));

                            let row_h = 34.0;
                            let (rect, resp) = ui.allocate_exact_size(
                                vec2(ui.available_width(), row_h),
                                Sense::click(),
                            );
                            let painter = ui.painter();

                            let bg = if is_added {
                                Color32::from_rgba_unmultiplied(
                                    p.accent.r(),
                                    p.accent.g(),
                                    p.accent.b(),
                                    32,
                                )
                            } else if resp.hovered() {
                                p.crt_widget_hover
                            } else {
                                p.crt_recess
                            };
                            let border = if is_added || resp.hovered() {
                                p.accent
                            } else {
                                p.crt_widget_hover
                            };

                            painter.rect_filled(rect, 3.0, bg);
                            painter.rect_stroke(
                                rect,
                                3.0,
                                Stroke::new(1.1, border),
                                StrokeKind::Inside,
                            );

                            // Nombre del ejecutable (arriba) + título de ventana (abajo)
                            painter.text(
                                pos2(rect.left() + 9.0, rect.top() + 11.0),
                                Align2::LEFT_CENTER,
                                &item.exe_name,
                                FontId::monospace(12.0),
                                if is_added { p.accent_bright } else { p.crt_text },
                            );

                            let title_short = truncate_chars(&item.window_title, 46);
                            painter.text(
                                pos2(rect.left() + 9.0, rect.bottom() - 9.5),
                                Align2::LEFT_CENTER,
                                title_short,
                                FontId::monospace(10.0),
                                p.crt_muted,
                            );

                            // Insignia derecha: AGREGADO / + AGREGAR
                            let badge_txt = if is_added { "AGREGADO ×" } else { "+ AGREGAR" };
                            let badge_col = if is_added { p.accent_bright } else { p.amber };
                            painter.text(
                                pos2(rect.right() - 10.0, rect.center().y),
                                Align2::RIGHT_CENTER,
                                badge_txt,
                                FontId::monospace(10.5),
                                badge_col,
                            );

                            if resp
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                clicked_exe = Some(item.exe_name.clone());
                            }
                            ui.add_space(3.0);
                        }
                    });
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Clic para agregar/quitar. Recuerda pulsar GUARDAR.")
                        .size(10.2)
                        .color(p.crt_muted),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if retro_pill(ui, p, true, "LISTO", vec2(74.0, 24.0)).clicked() {
                        app.show_exe_picker = false;
                    }
                });
            });
        });

    if let Some(exe) = clicked_exe {
        app.toggle_extra_exe(&exe);
    }
    if !keep_open {
        app.show_exe_picker = false;
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}
