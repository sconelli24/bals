use std::path::PathBuf;

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::ui::theme::RetroPalette;
use crate::ui::widgets::{
    SectionIcon, TimelineDragAction, draw_dual_trim_timeline, draw_floppy_icon, draw_scanlines,
    retro_card, retro_pill, retro_play_pill, retro_transport_icon_pill,
};
use crate::ui::{BalsUiApp, open_in_shell};

/// Pestaña 2 — Archivo de Clips (disquetes) + Monitor CRT con Preview Real & Deck Recortador (Opción C).
pub fn render_clips_tab(
    app: &mut BalsUiApp,
    ui: &mut egui::Ui,
    p: &RetroPalette,
    running_op: Option<&str>,
) {
    let total_w = ui.available_width();
    let left_w = (total_w * 0.39).clamp(240.0, 340.0);
    let right_w = (total_w - left_w - 10.0).max(320.0);
    let avail_h = ui.available_height();

    ui.horizontal_top(|ui| {
        // Columna izquierda: Archivo de clips estilo disquetes retro
        ui.allocate_ui_with_layout(
            vec2(left_w, avail_h),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui| {
                render_clips_archive(app, ui, p);
            },
        );

        ui.add_space(4.0);

        // Columna derecha: Monitor CRT 16:9 + Timeline dual + Botonera Deck
        ui.allocate_ui_with_layout(
            vec2(right_w, avail_h),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui| {
                render_crt_deck(app, ui, p, running_op);
            },
        );
    });
}

fn render_clips_archive(app: &mut BalsUiApp, ui: &mut egui::Ui, p: &RetroPalette) {
    let badge = format!("{} MP4", app.clips.len());
    retro_card(
        ui,
        p,
        Some(SectionIcon::Clips),
        "ARCHIVO DE CLIPS",
        Some(&badge),
        |ui| {
        ui.set_min_height((ui.available_height() - 4.0).max(160.0));
        // Controles rápidos: filtro + recargar + abrir carpeta
        ui.horizontal(|ui| {
            let filter_w = (ui.available_width() - 144.0).max(80.0);
            ui.add(
                egui::TextEdit::singleline(&mut app.clip_filter)
                    .hint_text("Filtrar...")
                    .desired_width(filter_w),
            );
            ui.add_enabled_ui(!app.clips_loading, |ui| {
                let rec_lbl = if app.clips_loading { "..." } else { "RECARGAR" };
                if retro_pill(ui, p, false, rec_lbl, vec2(68.0, 22.0))
                    .on_hover_text("Actualizar lista de clips")
                    .clicked()
                {
                    app.refresh_clips();
                }
            });
            if retro_pill(ui, p, false, "CARPETA", vec2(62.0, 22.0))
                .on_hover_text("Abrir carpeta de clips en el Explorador")
                .clicked()
            {
                let _ = std::fs::create_dir_all(&app.cfg.clips_dir);
                open_in_shell(&app.cfg.clips_dir);
            }
        });

        ui.add_space(6.0);

        let filter_lower = app.clip_filter.trim().to_ascii_lowercase();
        let filtered: Vec<_> = app
            .clips
            .iter()
            .filter(|c| {
                filter_lower.is_empty()
                    || c.name.to_ascii_lowercase().contains(&filter_lower)
                    || c.folder
                        .as_ref()
                        .is_some_and(|f| f.to_ascii_lowercase().contains(&filter_lower))
            })
            .cloned()
            .collect();

        if filtered.is_empty() {
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(if app.clips_loading && app.clips.is_empty() {
                    "ESCANEANDO CLIPS...".to_string()
                } else if app.clips.is_empty() {
                    format!(
                        "SIN CLIPS TODAVÍA.\nPresiona [ {} ] mientras juegas.\n\n{}",
                        app.cfg.save_clip_hotkey,
                        app.cfg.clips_dir.display()
                    )
                } else {
                    "Sin coincidencias para el filtro.".to_string()
                })
                .size(11.5)
                .color(p.crt_muted),
            );
            return;
        }

        let mut clicked_path: Option<PathBuf> = None;
        let scroll_h = (ui.available_height() - 4.0).max(160.0);
        egui::ScrollArea::vertical()
            .max_height(scroll_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for clip in &filtered {
                    let is_selected = app.selected_clip.as_ref() == Some(&clip.path);
                    let card_h = 36.0;
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(ui.available_width(), card_h), Sense::click());
                    let painter = ui.painter();

                    let bg = if is_selected {
                        Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 34)
                    } else if resp.hovered() {
                        p.crt_widget_hover
                    } else {
                        p.crt_recess
                    };
                    let border = if is_selected {
                        p.accent_bright
                    } else if resp.hovered() {
                        p.accent
                    } else {
                        p.crt_widget_hover
                    };

                    if is_selected {
                        let glow = Color32::from_rgba_unmultiplied(
                            p.accent.r(),
                            p.accent.g(),
                            p.accent.b(),
                            28,
                        );
                        painter.rect_filled(rect.expand(1.0), 4.0, glow);
                    }
                    painter.rect_filled(rect, 3.0, bg);
                    painter.rect_stroke(
                        rect,
                        3.0,
                        Stroke::new(if is_selected { 1.5 } else { 1.0 }, border),
                        StrokeKind::Inside,
                    );

                    // Ícono de disquete retro
                    let icon_rect = Rect::from_min_size(
                        pos2(rect.left() + 8.0, rect.center().y - 10.0),
                        vec2(18.0, 20.0),
                    );
                    draw_floppy_icon(painter, icon_rect, p, is_selected);

                    // Nombre truncado + tamaño MB a la derecha
                    let size_txt = format!("{:.1} MB", clip.size_mb);
                    painter.text(
                        pos2(rect.right() - 8.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        &size_txt,
                        FontId::monospace(11.0),
                        if is_selected { p.accent_bright } else { p.crt_muted },
                    );

                    let max_chars = ((rect.width() - 102.0) / 7.2).max(10.0) as usize;
                    let disp_name = truncate_middle(&clip.name, max_chars);
                    if let Some(folder) = &clip.folder {
                        painter.text(
                            pos2(icon_rect.right() + 8.0, rect.top() + 11.5),
                            Align2::LEFT_CENTER,
                            disp_name,
                            FontId::monospace(11.2),
                            if is_selected { p.crt_text } else { p.crt_text.linear_multiply(0.9) },
                        );
                        painter.text(
                            pos2(icon_rect.right() + 8.0, rect.bottom() - 9.5),
                            Align2::LEFT_CENTER,
                            folder,
                            FontId::monospace(9.8),
                            p.amber,
                        );
                    } else {
                        painter.text(
                            pos2(icon_rect.right() + 8.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            disp_name,
                            FontId::monospace(11.5),
                            if is_selected { p.crt_text } else { p.crt_text.linear_multiply(0.9) },
                        );
                    }

                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        clicked_path = Some(clip.path.clone());
                    }
                    if resp.double_clicked() {
                        open_in_shell(&clip.path);
                    }
                    ui.add_space(4.0);
                }
            });

        if let Some(path) = clicked_path {
            app.select_clip(path);
        }
    });
}

fn render_crt_deck(
    app: &mut BalsUiApp,
    ui: &mut egui::Ui,
    p: &RetroPalette,
    running_op: Option<&str>,
) {
    let meta_badge = app.selected_metadata.map(|m| {
        if m.width > 0 && m.fps > 0.0 {
            format!("{}p · {:.0}fps", m.height, m.fps.round())
        } else {
            format!("{:.1}s", m.duration_secs)
        }
    });

    retro_card(
        ui,
        p,
        Some(SectionIcon::Deck),
        "MONITOR CRT & RECORTADOR",
        meta_badge.as_deref(),
        |ui| {
            let card_body_h = (ui.available_height() - 4.0).max(320.0);
            ui.set_min_height(card_body_h);

            let Some(sel_path) = app.selected_clip.clone() else {
                ui.add_space((card_body_h * 0.42).max(40.0));
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("[ NO SIGNAL ]\nSelecciona un clip del archivo.")
                            .size(13.0)
                            .color(p.crt_muted),
                    );
                });
                return;
            };

            // 1. Monitor CRT expandido al alto disponible del deck
            let gap = ui.spacing().item_spacing.y;
            let controls_h = if app.selected_duration.is_some() {
                // add_space(5) + transport(24) + add_space(4) + timeline(44)
                // + fine_adjust(20) + add_space(8) + actions_row1(30)
                // + add_space(5) + compress_btn(27) + 5 item gaps + 2px margin
                169.0 + gap * 5.0
            } else {
                // add_space(5) + status_label(18) + add_space(8)
                // + actions_row1(30) + add_space(5) + compress_btn(27) + 3 item gaps + 2px margin
                95.0 + gap * 3.0
            } + if running_op.is_some() {
                17.0 + gap
            } else {
                0.0
            };
            let mon_w = ui.available_width();
            let mon_h = (card_body_h - controls_h).max(180.0);
            let (mon_rect, mon_resp) =
                ui.allocate_exact_size(vec2(mon_w, mon_h), Sense::click());
            let painter = ui.painter();

            // Marco oscuro biselado del monitor CRT
            painter.rect_filled(mon_rect, 4.0, p.crt_recess);
            painter.rect_stroke(
                mon_rect,
                4.0,
                Stroke::new(2.0, p.crt_panel_border),
                StrokeKind::Inside,
            );

            let screen_rect = mon_rect.shrink(4.0);
            let video_size = app
                .nv12_renderer
                .as_ref()
                .and_then(|r| r.frame_size())
                .map(|(w, h)| vec2(w as f32, h as f32))
                .or_else(|| app.preview_texture.as_ref().map(|t| t.size_vec2()));

            if let Some(vid_sz) = video_size {
                let aspect = (vid_sz.x / vid_sz.y.max(1.0)).max(0.1);
                let fit_w = (screen_rect.height() * aspect).min(screen_rect.width());
                let fit_h = fit_w / aspect;
                let img_rect = Rect::from_center_size(screen_rect.center(), vec2(fit_w, fit_h));
                if let Some(r) = app.nv12_renderer.as_ref().filter(|r| r.frame_size().is_some()) {
                    painter.add(r.paint_callback(img_rect, app.clip_player.buffer_pool().clone()));
                } else if let Some(tex) = &app.preview_texture {
                    painter.image(
                        tex.id(),
                        img_rect,
                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
            } else {
                let msg = if app.preview_loading {
                    "CARGANDO CLIP EN MONITOR CRT..."
                } else {
                    "VISTA PREVIA NO DISPONIBLE"
                };
                painter.text(
                    screen_rect.center(),
                    Align2::CENTER_CENTER,
                    msg,
                    FontId::monospace(12.0),
                    p.crt_muted,
                );
            }

            if app.cfg.crt_scanlines {
                draw_scanlines(painter, screen_rect, p.crt_scanline);
            }

            // Overlays estilo VCR (arriba izq = timecode + estado + backend, arriba der = nombre)
            let is_playing = app.clip_player.is_playing();
            let total_dur = app.selected_duration.unwrap_or(0.0);
            let state_tag = if is_playing {
                if app.player_loop { "LOOP" } else { "PLAY" }
            } else {
                "PAUSA"
            };
            let pipe_tag = match (app.clip_player.is_hw_accelerated(), app.clip_player.is_nv12()) {
                (true, true) => "GPU NV12",
                (true, false) => "GPU RGB",
                (false, true) => "CPU NV12",
                (false, false) => "CPU RGB",
            };
            let tc_text = format!(
                "{state_tag} · {} / {} · {pipe_tag}",
                format_timecode(app.preview_time_secs),
                format_timecode(total_dur)
            );
            draw_vcr_badge(
                painter,
                pos2(screen_rect.left() + 8.0, screen_rect.top() + 8.0),
                &tc_text,
                p,
                false,
            );

            let fname = sel_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            draw_vcr_badge(
                painter,
                pos2(screen_rect.right() - 8.0, screen_rect.top() + 8.0),
                &truncate_middle(&fname, 26),
                p,
                true,
            );

            let mon_resp = mon_resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Clic: Play / Pausa (Espacio) · Doble clic: abrir en reproductor externo");
            if mon_resp.double_clicked() {
                app.clip_player.pause();
                open_in_shell(&sel_path);
            } else if mon_resp.clicked() {
                app.clip_player.toggle_play();
            }

            ui.add_space(5.0);

            // 1b. Barra compacta del reproductor nativo (Play/Pausa, paso rápido, Marcar IN/OUT, Loop, Volumen)
            if app.selected_duration.is_some() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.spacing_mut().slider_width = 64.0;

                    if retro_transport_icon_pill(ui, p, is_playing, vec2(74.0, 24.0))
                        .on_hover_text("Reproducir o pausar clip (Espacio)")
                        .clicked()
                    {
                        app.clip_player.toggle_play();
                    }

                    if retro_pill(ui, p, false, "-1s", vec2(32.0, 24.0))
                        .on_hover_text("Retroceder 1 segundo (Flecha izquierda · Shift: 0.1s)")
                        .clicked()
                    {
                        app.step_player_secs(-1.0);
                    }
                    if retro_pill(ui, p, false, "+1s", vec2(32.0, 24.0))
                        .on_hover_text("Avanzar 1 segundo (Flecha derecha · Shift: 0.1s)")
                        .clicked()
                    {
                        app.step_player_secs(1.0);
                    }

                    ui.add_space(2.0);
                    if retro_pill(ui, p, false, "MARCAR IN", vec2(76.0, 24.0))
                        .on_hover_text("Fijar inicio del recorte [IN] en el frame actual (Tecla I)")
                        .clicked()
                    {
                        app.mark_trim_in_at_playhead();
                    }
                    if retro_pill(ui, p, false, "MARCAR OUT", vec2(82.0, 24.0))
                        .on_hover_text("Fijar fin del recorte [OUT] en el frame actual (Tecla O)")
                        .clicked()
                    {
                        app.mark_trim_out_at_playhead();
                    }

                    ui.add_space(2.0);
                    if retro_pill(ui, p, app.player_loop, "LOOP", vec2(46.0, 24.0))
                        .on_hover_text("Reproducir en bucle solo el tramo [IN - OUT] (Tecla L)")
                        .clicked()
                    {
                        app.player_loop = !app.player_loop;
                        app.sync_player_loop();
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let has_audio = app.clip_player.has_audio();
                        ui.add_enabled_ui(has_audio, |ui| {
                            if ui
                                .add(
                                    egui::Slider::new(&mut app.player_volume, 0.0..=1.0)
                                        .show_value(false),
                                )
                                .on_hover_text(format!(
                                    "Volumen del reproductor: {:.0}%",
                                    app.player_volume * 100.0
                                ))
                                .changed()
                            {
                                app.clip_player.set_volume(app.player_volume);
                            }
                            let mute_lbl = if app.player_muted { "MUTE" } else { "VOL" };
                            if retro_pill(ui, p, !app.player_muted && has_audio, mute_lbl, vec2(44.0, 24.0))
                                .on_hover_text("Silenciar / activar audio del clip (Tecla M)")
                                .clicked()
                            {
                                app.player_muted = !app.player_muted;
                                app.clip_player.set_muted(app.player_muted);
                            }
                        });
                    });
                });

                ui.add_space(4.0);
            }

            // 2. Línea de tiempo dual interactiva [IN] / [OUT] + cabezal clickeable
            if let Some(total) = app.selected_duration {
                if let Some(action) = draw_dual_trim_timeline(
                    ui,
                    p,
                    total,
                    app.preview_time_secs,
                    &mut app.trim_start,
                    &mut app.trim_end,
                    &mut app.trim_drag_handle,
                ) {
                    match action {
                        TimelineDragAction::TrimSeek(seek_t) => {
                            app.sync_player_loop();
                            app.schedule_preview_seek(seek_t);
                        }
                        TimelineDragAction::PlayheadSeek(seek_t) => {
                            app.schedule_preview_seek(seek_t);
                        }
                    }
                }

                // Controles de ajuste fino (-0.5s / +0.5s) y resumen de duración
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("IN: {:.1}s", app.trim_start))
                            .size(11.5)
                            .color(p.accent),
                    );
                    if retro_pill(ui, p, false, "-.5", vec2(32.0, 20.0)).clicked() {
                        app.trim_start = (app.trim_start - 0.5).max(0.0);
                        app.sync_player_loop();
                        app.seek_now(app.trim_start);
                    }
                    if retro_pill(ui, p, false, "+.5", vec2(32.0, 20.0)).clicked() {
                        app.trim_start = (app.trim_start + 0.5).min((app.trim_end - 0.5).max(0.0));
                        app.sync_player_loop();
                        app.seek_now(app.trim_start);
                    }

                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("OUT: {:.1}s", app.trim_end))
                            .size(11.5)
                            .color(p.amber),
                    );
                    if retro_pill(ui, p, false, "-.5", vec2(32.0, 20.0)).clicked() {
                        app.trim_end = (app.trim_end - 0.5).max((app.trim_start + 0.5).min(total));
                        app.sync_player_loop();
                        app.seek_now(app.trim_end);
                    }
                    if retro_pill(ui, p, false, "+.5", vec2(32.0, 20.0)).clicked() {
                        app.trim_end = (app.trim_end + 0.5).min(total);
                        app.sync_player_loop();
                        app.seek_now(app.trim_end);
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "CORTE: {:.1}s",
                                (app.trim_end - app.trim_start).max(0.0)
                            ))
                            .strong()
                            .size(11.5)
                            .color(p.crt_text),
                        );
                    });
                });
            } else if app.preview_loading {
                ui.label(
                    egui::RichText::new("LEYENDO METADATOS DEL CLIP...")
                        .size(11.5)
                        .color(p.amber),
                );
            } else {
                ui.label(
                    egui::RichText::new("No se pudo leer la duración del archivo.")
                        .size(11.5)
                        .color(p.led_red),
                );
            }

            ui.add_space(8.0);

            // 3. Botonera física Deck (Abrir externo, Recorte Passthrough, Copiar Ruta, Borrar, Comprimir)
            let trim_in_progress = running_op.is_some();
            ui.add_enabled_ui(!trim_in_progress, |ui| {
                let row_w = ui.available_width();
                ui.horizontal(|ui| {
                    if retro_play_pill(ui, p, "ABRIR EXT", vec2(96.0, 30.0))
                        .on_hover_text("Abrir en tu reproductor externo de video (ej. mpv)")
                        .clicked()
                    {
                        app.clip_player.pause();
                        open_in_shell(&sel_path);
                    }

                    let trim_w = (row_w - 296.0).max(150.0);
                    let trim_lbl = running_op.unwrap_or("RECORTAR SIN PÉRDIDA");
                    if retro_pill(ui, p, true, trim_lbl, vec2(trim_w, 30.0))
                        .on_hover_text("Corte instantáneo MP4 passthrough sin recodificar")
                        .clicked()
                    {
                        app.start_trim();
                    }

                    if retro_pill(ui, p, false, "COPIAR RUTA", vec2(92.0, 30.0))
                        .on_hover_text("Copiar ruta completa del clip al portapapeles")
                        .clicked()
                    {
                        ui.ctx().copy_text(sel_path.to_string_lossy().into_owned());
                        app.set_banner("Ruta del clip copiada al portapapeles.", false);
                    }

                    // Botón eliminar con confirmación en 2 clics
                    let confirming = app.confirm_delete.as_ref() == Some(&sel_path);
                    let del_lbl = if confirming { "¿CONFIRMAR?" } else { "BORRAR" };
                    let (del_rect, del_resp) =
                        ui.allocate_exact_size(vec2(84.0, 30.0), Sense::click());
                    let del_bg = if confirming {
                        p.danger
                    } else if del_resp.hovered() {
                        Color32::from_rgba_unmultiplied(p.danger.r(), p.danger.g(), p.danger.b(), 150)
                    } else {
                        p.crt_widget_bg
                    };
                    ui.painter().rect_filled(del_rect, 3.0, del_bg);
                    ui.painter().rect_stroke(
                        del_rect,
                        3.0,
                        Stroke::new(1.2, if confirming { p.led_red } else { p.crt_widget_hover }),
                        StrokeKind::Inside,
                    );
                    ui.painter().text(
                        del_rect.center(),
                        Align2::CENTER_CENTER,
                        del_lbl,
                        FontId::monospace(11.0),
                        if confirming { p.danger_text } else { p.crt_text },
                    );
                    if del_resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(if confirming {
                            "Clic nuevamente para confirmar eliminación permanente"
                        } else {
                            "Eliminar clip seleccionado"
                        })
                        .clicked()
                    {
                        if confirming {
                            app.clip_player.release_clip_sync();
                            app.player_clip_released = true;
                            if let Some(r) = &app.nv12_renderer {
                                r.clear(app.clip_player.buffer_pool());
                            }
                            app.preview_texture = None;
                            if std::fs::remove_file(&sel_path).is_ok() {
                                app.confirm_delete = None;
                                app.set_banner("Clip eliminado permanentemente.", false);
                                app.refresh_clips();
                            }
                        } else {
                            app.confirm_delete = Some(sel_path.clone());
                        }
                    }
                });

                ui.add_space(5.0);

                if retro_pill(
                    ui,
                    p,
                    false,
                    "COMPRIMIR CLIP (~35% MENOS PESO · ALTA CALIDAD)",
                    vec2(row_w, 27.0),
                )
                .on_hover_text(
                    "Reduce ~35% el peso manteniendo resolución y FPS originales sin perder calidad visual",
                )
                .clicked()
                {
                    app.start_compress_balanced();
                }
            });

            if let Some(op_label) = running_op {
                ui.add_space(5.0);
                let (bar_rect, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 12.0), Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(bar_rect, 2.0, p.crt_recess);
                painter.rect_stroke(
                    bar_rect,
                    2.0,
                    Stroke::new(1.0, p.crt_widget_hover),
                    StrokeKind::Inside,
                );
                let t = ui.ctx().input(|i| i.time) as f32;
                let phase = (t * 1.4).fract();
                let seg_w = (bar_rect.width() * 0.28).max(32.0);
                let x = bar_rect.left() + (bar_rect.width() - seg_w) * phase;
                let seg_rect = Rect::from_min_size(
                    pos2(x, bar_rect.top() + 2.0),
                    vec2(seg_w, bar_rect.height() - 4.0),
                );
                painter.rect_filled(seg_rect, 1.5, p.amber);
                painter.text(
                    bar_rect.center(),
                    Align2::CENTER_CENTER,
                    op_label,
                    FontId::monospace(9.5),
                    p.crt_text,
                );
            }
        },
    );
}

fn draw_vcr_badge(
    painter: &egui::Painter,
    anchor: egui::Pos2,
    text: &str,
    p: &RetroPalette,
    align_right: bool,
) {
    let w = text.chars().count() as f32 * 6.8 + 12.0;
    let h = 18.0;
    let rect = if align_right {
        Rect::from_min_size(pos2(anchor.x - w, anchor.y), vec2(w, h))
    } else {
        Rect::from_min_size(anchor, vec2(w, h))
    };
    painter.rect_filled(rect, 2.0, Color32::from_rgba_unmultiplied(8, 12, 10, 200));
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 110)),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::monospace(10.5),
        p.crt_text,
    );
}

fn format_timecode(secs: f64) -> String {
    let clamped = secs.max(0.0);
    let mins = (clamped / 60.0).floor() as u32;
    let rem = clamped - (mins as f64 * 60.0);
    format!("{mins:02}:{rem:04.1}")
}

fn truncate_middle(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars || max_chars < 6 {
        return s.to_string();
    }
    let head = (max_chars - 2) / 2;
    let tail = max_chars - 2 - head;
    let left: String = chars[..head].iter().collect();
    let right: String = chars[chars.len() - tail..].iter().collect();
    format!("{left}..{right}")
}
