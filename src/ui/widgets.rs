use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    Vec2, pos2, vec2,
};

use crate::config::CameraCorner;
use crate::ui::theme::RetroPalette;

/// Dibuja una superficie táctil con bisel estilo hardware retro (luz arriba-izquierda, sombra abajo-derecha).
pub fn draw_beveled_box(
    painter: &egui::Painter,
    rect: Rect,
    bg: Color32,
    highlight: Color32,
    shadow: Color32,
    outline: Color32,
    radius: f32,
) {
    painter.rect_filled(rect, radius, bg);

    // Bisel superior e izquierdo (luz)
    let inner = rect.shrink(1.5);
    if inner.width() > 4.0 && inner.height() > 4.0 {
        painter.line_segment(
            [pos2(inner.left(), inner.top()), pos2(inner.right(), inner.top())],
            Stroke::new(1.5, highlight),
        );
        painter.line_segment(
            [pos2(inner.left(), inner.top()), pos2(inner.left(), inner.bottom())],
            Stroke::new(1.5, highlight),
        );
        // Bisel inferior y derecho (sombra)
        painter.line_segment(
            [
                pos2(inner.left(), inner.bottom()),
                pos2(inner.right(), inner.bottom()),
            ],
            Stroke::new(1.5, shadow),
        );
        painter.line_segment(
            [
                pos2(inner.right(), inner.top()),
                pos2(inner.right(), inner.bottom()),
            ],
            Stroke::new(1.5, shadow),
        );
    }

    painter.rect_stroke(rect, radius, Stroke::new(1.5, outline), StrokeKind::Inside);
}

/// Interpola linealmente dos colores sRGB para generar sombreados del chasis en cualquier tema.
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 { (x as f32 + (y as f32 - x as f32) * t).round() as u8 };
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Dibuja la carcasa exterior continua del monitor CRT con bisel moldeado 3D en los 4 bordes.
pub fn draw_crt_monitor_shell(painter: &egui::Painter, rect: Rect, p: &RetroPalette) {
    // Base del chasis plástico
    painter.rect_filled(rect, 0.0, p.chassis_bg);

    // Marco moldeado exterior marcado (luz arriba/izquierda, sombra profunda abajo/derecha)
    let bevel = rect.shrink(2.0);
    let hi_soft = lerp_color(p.chassis_bg, p.chassis_highlight, 0.92);
    let sh_soft = lerp_color(p.chassis_bg, p.chassis_shadow, 0.92);
    let sh_deep = lerp_color(p.chassis_shadow, p.chassis_outline, 0.38);

    // Bisel exterior superior e izquierdo (luz)
    painter.line_segment(
        [pos2(bevel.left(), bevel.top()), pos2(bevel.right(), bevel.top())],
        Stroke::new(4.0, hi_soft),
    );
    painter.line_segment(
        [pos2(bevel.left(), bevel.top()), pos2(bevel.left(), bevel.bottom())],
        Stroke::new(4.0, hi_soft),
    );
    // Bisel exterior inferior y derecho (sombra profunda)
    painter.line_segment(
        [pos2(bevel.left(), bevel.bottom()), pos2(bevel.right(), bevel.bottom())],
        Stroke::new(4.5, sh_deep),
    );
    painter.line_segment(
        [pos2(bevel.right(), bevel.top()), pos2(bevel.right(), bevel.bottom())],
        Stroke::new(4.0, sh_soft),
    );

    // Doble línea de moldeo interior sutil (sensación de carcasa inyectada en plástico)
    let seam = rect.shrink(6.0);
    painter.rect_stroke(
        seam,
        3.0,
        Stroke::new(1.2, lerp_color(p.chassis_bg, p.chassis_shadow, 0.34)),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        seam.shrink(1.5),
        2.5,
        Stroke::new(1.0, lerp_color(p.chassis_bg, p.chassis_highlight, 0.38)),
        StrokeKind::Inside,
    );

    // Contorno exterior nítido
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.5, p.chassis_outline),
        StrokeKind::Inside,
    );
}

/// Dibuja un hueco empotrado oscuro en el plástico del chasis (LCD, VU meter, bahía de switches).
pub fn draw_recessed_bay(painter: &egui::Painter, rect: Rect, p: &RetroPalette, radius: f32) {
    // Borde exterior hundido (sombra arriba/izq, brillo abajo/der)
    let outer = rect.expand(1.2);
    painter.line_segment(
        [pos2(outer.left() + radius, outer.top()), pos2(outer.right() - radius, outer.top())],
        Stroke::new(1.3, p.chassis_shadow),
    );
    painter.line_segment(
        [pos2(outer.left(), outer.top() + radius), pos2(outer.left(), outer.bottom() - radius)],
        Stroke::new(1.3, p.chassis_shadow),
    );
    painter.line_segment(
        [pos2(outer.left() + radius, outer.bottom()), pos2(outer.right() - radius, outer.bottom())],
        Stroke::new(1.3, p.chassis_highlight),
    );
    painter.line_segment(
        [pos2(outer.right(), outer.top() + radius), pos2(outer.right(), outer.bottom() - radius)],
        Stroke::new(1.3, p.chassis_highlight),
    );

    painter.rect_filled(rect, radius, p.crt_recess);
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.5, p.chassis_outline),
        StrokeKind::Inside,
    );
}

/// Dibuja líneas de barrido horizontales CRT sutiles sin interferir con clics.
pub fn draw_scanlines(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.0, color);
    let mut y = rect.top() + 1.0;
    while y < rect.bottom() {
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], stroke);
        y += 3.0;
    }
}

/// Construye un polígono convexo en sentido horario con esquinas redondeadas y suave
/// curvatura *barrel* hacia afuera, replicando la geometría de un tubo de vidrio CRT real.
fn build_crt_curved_polygon(rect: Rect, corner_r: f32, bulge_x: f32, bulge_y: f32) -> Vec<Pos2> {
    use std::f32::consts::{FRAC_PI_2, PI};

    let r = corner_r.min(rect.width() * 0.22).min(rect.height() * 0.22).max(2.0);
    let edge_steps = 8usize;
    let corner_steps = 6usize;
    let mut pts = Vec::with_capacity((edge_steps + corner_steps) * 4);

    // Borde superior (izq -> der, curvado hacia arriba)
    for i in 1..edge_steps {
        let t = i as f32 / edge_steps as f32;
        let x = rect.left() + r + (rect.width() - 2.0 * r) * t;
        let y = rect.top() - bulge_y * (PI * t).sin();
        pts.push(pos2(x, y));
    }
    // Esquina superior derecha (-PI/2 -> 0)
    let tr_c = pos2(rect.right() - r, rect.top() + r);
    for i in 0..=corner_steps {
        let a = -FRAC_PI_2 + FRAC_PI_2 * (i as f32 / corner_steps as f32);
        pts.push(pos2(tr_c.x + r * a.cos(), tr_c.y + r * a.sin()));
    }
    // Borde derecho (arriba -> abajo, curvado hacia afuera)
    for i in 1..edge_steps {
        let t = i as f32 / edge_steps as f32;
        let x = rect.right() + bulge_x * (PI * t).sin();
        let y = rect.top() + r + (rect.height() - 2.0 * r) * t;
        pts.push(pos2(x, y));
    }
    // Esquina inferior derecha (0 -> PI/2)
    let br_c = pos2(rect.right() - r, rect.bottom() - r);
    for i in 0..=corner_steps {
        let a = FRAC_PI_2 * (i as f32 / corner_steps as f32);
        pts.push(pos2(br_c.x + r * a.cos(), br_c.y + r * a.sin()));
    }
    // Borde inferior (der -> izq, curvado hacia abajo)
    for i in 1..edge_steps {
        let t = i as f32 / edge_steps as f32;
        let x = rect.right() - r - (rect.width() - 2.0 * r) * t;
        let y = rect.bottom() + bulge_y * (PI * t).sin();
        pts.push(pos2(x, y));
    }
    // Esquina inferior izquierda (PI/2 -> PI)
    let bl_c = pos2(rect.left() + r, rect.bottom() - r);
    for i in 0..=corner_steps {
        let a = FRAC_PI_2 + FRAC_PI_2 * (i as f32 / corner_steps as f32);
        pts.push(pos2(bl_c.x + r * a.cos(), bl_c.y + r * a.sin()));
    }
    // Borde izquierdo (abajo -> arriba, curvado hacia afuera)
    for i in 1..edge_steps {
        let t = i as f32 / edge_steps as f32;
        let x = rect.left() - bulge_x * (PI * t).sin();
        let y = rect.bottom() - r - (rect.height() - 2.0 * r) * t;
        pts.push(pos2(x, y));
    }
    // Esquina superior izquierda (PI -> 3*PI/2)
    let tl_c = pos2(rect.left() + r, rect.top() + r);
    for i in 0..=corner_steps {
        let a = PI + FRAC_PI_2 * (i as f32 / corner_steps as f32);
        pts.push(pos2(tl_c.x + r * a.cos(), tl_c.y + r * a.sin()));
    }

    pts
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let f = |x: f32, y: f32| x + (y - x) * t;
    Rect::from_min_max(
        pos2(f(a.left(), b.left()), f(a.top(), b.top())),
        pos2(f(a.right(), b.right()), f(a.bottom(), b.bottom())),
    )
}

/// Dibuja la cavidad 3D biselada profunda y el tubo de vidrio curvo de un monitor CRT retro.
/// Devuelve el rectángulo del vidrio interior seguro donde se renderiza el contenido.
pub fn draw_crt_recess(painter: &egui::Painter, rect: Rect, p: &RetroPalette) -> Rect {
    // Embudo biselado profundo (inspirado en monitores CRT clásicos)
    let chamfer_x = 22.0;
    let chamfer_y = 18.0;
    let glass_outer = Rect::from_min_max(
        pos2(rect.left() + chamfer_x, rect.top() + chamfer_y),
        pos2(rect.right() - chamfer_x, rect.bottom() - chamfer_y),
    );

    // 1. Paredes trapezoidales en 3 bandas graduadas de profundidad hacia el tubo
    let top_outer = lerp_color(p.chassis_shadow, p.chassis_outline, 0.35);
    let top_inner = lerp_color(p.chassis_shadow, p.chassis_outline, 0.72);
    let left_outer = lerp_color(p.chassis_bg, p.chassis_shadow, 0.72);
    let left_inner = lerp_color(p.chassis_shadow, p.chassis_outline, 0.42);
    let right_outer = lerp_color(p.chassis_bg, p.chassis_shadow, 0.46);
    let right_inner = lerp_color(p.chassis_shadow, p.chassis_outline, 0.30);
    let bottom_outer = lerp_color(p.chassis_bg, p.chassis_highlight, 0.48);
    let bottom_inner = lerp_color(p.chassis_bg, p.chassis_shadow, 0.34);

    let bands = [(0.0f32, 0.36f32, 0.0f32), (0.36, 0.72, 0.5), (0.72, 1.0, 1.0)];
    for (t0, t1, shade) in bands {
        let r0 = lerp_rect(rect, glass_outer, t0);
        let r1 = lerp_rect(rect, glass_outer, t1);

        painter.add(egui::Shape::convex_polygon(
            vec![r0.left_top(), r0.right_top(), r1.right_top(), r1.left_top()],
            lerp_color(top_outer, top_inner, shade),
            Stroke::NONE,
        ));
        painter.add(egui::Shape::convex_polygon(
            vec![r0.left_top(), r1.left_top(), r1.left_bottom(), r0.left_bottom()],
            lerp_color(left_outer, left_inner, shade),
            Stroke::NONE,
        ));
        painter.add(egui::Shape::convex_polygon(
            vec![r0.right_top(), r0.right_bottom(), r1.right_bottom(), r1.right_top()],
            lerp_color(right_outer, right_inner, shade),
            Stroke::NONE,
        ));
        painter.add(egui::Shape::convex_polygon(
            vec![r0.left_bottom(), r1.left_bottom(), r1.right_bottom(), r0.right_bottom()],
            lerp_color(bottom_outer, bottom_inner, shade),
            Stroke::NONE,
        ));
    }

    // Aristas diagonales en las 4 esquinas para acentuar la profundidad 3D del molde
    let crease_top = Stroke::new(1.2, lerp_color(p.chassis_shadow, p.chassis_outline, 0.48));
    let crease_bot = Stroke::new(1.2, lerp_color(p.chassis_shadow, p.chassis_outline, 0.28));
    painter.line_segment([rect.left_top(), glass_outer.left_top()], crease_top);
    painter.line_segment([rect.right_top(), glass_outer.right_top()], crease_top);
    painter.line_segment([rect.left_bottom(), glass_outer.left_bottom()], crease_bot);
    painter.line_segment([rect.right_bottom(), glass_outer.right_bottom()], crease_bot);

    // Labio exterior del hueco CRT (sombra arriba/izq + brillo inferior/der)
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.6, lerp_color(p.chassis_shadow, p.chassis_outline, 0.52)),
        StrokeKind::Inside,
    );
    painter.line_segment(
        [
            pos2(rect.left() + 2.0, rect.bottom() + 0.8),
            pos2(rect.right() - 2.0, rect.bottom() + 0.8),
        ],
        Stroke::new(1.6, p.chassis_highlight),
    );
    painter.line_segment(
        [
            pos2(rect.right() + 0.8, rect.top() + 2.0),
            pos2(rect.right() + 0.8, rect.bottom() - 2.0),
        ],
        Stroke::new(1.4, lerp_color(p.chassis_bg, p.chassis_highlight, 0.72)),
    );

    // 2. Tubo de vidrio CRT curvo empotrado (geometría barrel convexa)
    let outer_poly = build_crt_curved_polygon(glass_outer, 20.0, 5.0, 3.5);
    painter.add(egui::Shape::convex_polygon(
        outer_poly.clone(),
        Color32::from_rgb(10, 12, 12),
        Stroke::NONE,
    ));
    painter.add(egui::Shape::closed_line(
        outer_poly,
        Stroke::new(1.5, p.chassis_outline),
    ));

    let glass_inner = glass_outer.shrink(5.5);
    let inner_poly = build_crt_curved_polygon(glass_inner, 16.0, 4.0, 2.8);
    painter.add(egui::Shape::convex_polygon(
        inner_poly,
        p.crt_bg,
        Stroke::NONE,
    ));

    // Viñeta interior progresiva (sensación de curvatura del fósforo CRT y sombra del bisel)
    for (inset, alpha, width) in [(1.0f32, 150u8, 4.0f32), (4.0, 75, 3.0), (7.5, 32, 2.5)] {
        let vpoly = build_crt_curved_polygon(glass_inner.shrink(inset), (16.0 - inset).max(6.0), 3.6, 2.4);
        painter.add(egui::Shape::closed_line(
            vpoly,
            Stroke::new(width, Color32::from_rgba_unmultiplied(0, 0, 0, alpha)),
        ));
    }

    // Sombra proyectada superior (overhang del bisel plástico sobre el vidrio)
    let overhang_rect = Rect::from_min_max(
        pos2(glass_inner.left() + 10.0, glass_inner.top() + 1.0),
        pos2(glass_inner.right() - 10.0, glass_inner.top() + 8.0),
    );
    painter.rect_filled(
        overhang_rect,
        3.0,
        Color32::from_rgba_unmultiplied(0, 0, 0, 48),
    );

    // Reflejo cristalino curvo muy sutil en la parte superior del vidrio CRT
    let gloss_y = glass_inner.top() + 4.0;
    painter.line_segment(
        [
            pos2(glass_inner.left() + 26.0, gloss_y),
            pos2(glass_inner.right() - 26.0, gloss_y),
        ],
        Stroke::new(1.2, Color32::from_rgba_unmultiplied(255, 255, 255, 22)),
    );

    glass_inner.shrink2(vec2(5.0, 4.0))
}

/// Íconos vectoriales retro 14×14 para identificar visualmente cada tarjeta de sección.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionIcon {
    Video,
    Audio,
    Capture,
    Camera,
    System,
    Clips,
    Deck,
}

/// Dibuja un ícono vectorial retro nítido de 14×14 px acorde a la paleta CRT activa.
pub fn draw_section_icon(painter: &egui::Painter, rect: Rect, p: &RetroPalette, icon: SectionIcon) {
    let col = p.accent_bright;
    let stroke = Stroke::new(1.3, col);
    let r = rect.shrink(1.0);

    match icon {
        SectionIcon::Video => {
            // Cámara de video retro: cuerpo + lente triangular a la derecha + LED REC
            let body = Rect::from_min_max(
                pos2(r.left(), r.top() + 2.0),
                pos2(r.left() + r.width() * 0.66, r.bottom() - 2.0),
            );
            painter.rect_stroke(body, 1.5, stroke, StrokeKind::Inside);
            painter.circle_filled(
                pos2(body.left() + 3.0, body.top() + 3.0),
                1.3,
                p.led_red,
            );
            let lens_x = body.right() + 0.5;
            let lens = vec![
                pos2(lens_x, r.center().y - 1.5),
                pos2(r.right(), r.top() + 2.5),
                pos2(r.right(), r.bottom() - 2.5),
                pos2(lens_x, r.center().y + 1.5),
            ];
            painter.add(egui::Shape::convex_polygon(lens, col, Stroke::NONE));
        }
        SectionIcon::Audio => {
            // Parlante + dos barras/ondas sonoras
            let box_r = Rect::from_min_max(
                pos2(r.left(), r.center().y - 2.5),
                pos2(r.left() + 3.2, r.center().y + 2.5),
            );
            painter.rect_filled(box_r, 0.5, col);
            let cone = vec![
                pos2(box_r.right() - 0.5, r.center().y - 2.5),
                pos2(r.left() + 7.2, r.top() + 1.5),
                pos2(r.left() + 7.2, r.bottom() - 1.5),
                pos2(box_r.right() - 0.5, r.center().y + 2.5),
            ];
            painter.add(egui::Shape::convex_polygon(cone, col, Stroke::NONE));
            painter.line_segment(
                [
                    pos2(r.left() + 9.5, r.center().y - 2.6),
                    pos2(r.left() + 9.5, r.center().y + 2.6),
                ],
                Stroke::new(1.3, p.amber),
            );
            painter.line_segment(
                [
                    pos2(r.left() + 11.8, r.center().y - 4.4),
                    pos2(r.left() + 11.8, r.center().y + 4.4),
                ],
                Stroke::new(1.3, col),
            );
        }
        SectionIcon::Capture => {
            // Monitor con mira / captura central + base
            let screen = Rect::from_min_max(
                pos2(r.left(), r.top() + 1.0),
                pos2(r.right(), r.bottom() - 3.5),
            );
            painter.rect_stroke(screen, 1.5, stroke, StrokeKind::Inside);
            // Pequeño punto de captura en el centro de la pantalla
            painter.circle_filled(screen.center(), 1.8, p.amber);
            // Pie del monitor
            painter.line_segment(
                [
                    pos2(r.center().x, screen.bottom()),
                    pos2(r.center().x, r.bottom() - 1.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(r.center().x - 3.5, r.bottom() - 0.8),
                    pos2(r.center().x + 3.5, r.bottom() - 0.8),
                ],
                stroke,
            );
        }
        SectionIcon::Camera => {
            // Webcam retro circular con lente central, LED y soporte base
            let lens_center = pos2(r.center().x, r.center().y - 1.2);
            painter.circle_stroke(lens_center, 4.6, stroke);
            painter.circle_filled(lens_center, 1.8, p.amber);
            painter.line_segment(
                [
                    pos2(r.center().x - 3.6, r.bottom() - 0.8),
                    pos2(r.center().x + 3.6, r.bottom() - 0.8),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(r.center().x, lens_center.y + 4.4),
                    pos2(r.center().x, r.bottom() - 0.8),
                ],
                stroke,
            );
        }
        SectionIcon::System => {
            // Sliders / panel de ajustes retro (3 carriles con perillas)
            let ys = [r.top() + 2.5, r.center().y, r.bottom() - 2.5];
            let knobs = [r.left() + 3.5, r.right() - 3.5, r.center().x];
            for (y, kx) in ys.into_iter().zip(knobs) {
                painter.line_segment(
                    [pos2(r.left() + 0.5, y), pos2(r.right() - 0.5, y)],
                    Stroke::new(1.2, p.crt_muted),
                );
                painter.rect_filled(
                    Rect::from_center_size(pos2(kx, y), vec2(3.2, 3.2)),
                    0.5,
                    col,
                );
            }
        }
        SectionIcon::Clips => {
            // Mini disquete 3.5"
            painter.rect_stroke(r, 1.5, stroke, StrokeKind::Inside);
            let shutter = Rect::from_min_max(
                pos2(r.left() + 3.0, r.top() + 1.0),
                pos2(r.right() - 3.0, r.top() + 5.0),
            );
            painter.rect_filled(shutter, 0.5, col);
            let lbl = Rect::from_min_max(
                pos2(r.left() + 2.5, r.bottom() - 5.0),
                pos2(r.right() - 2.5, r.bottom() - 1.5),
            );
            painter.rect_filled(lbl, 0.5, p.amber);
        }
        SectionIcon::Deck => {
            // Mini reproductor / deck (triángulo Play dentro de pantalla)
            painter.rect_stroke(r, 1.8, stroke, StrokeKind::Inside);
            let play = vec![
                pos2(r.center().x - 2.0, r.center().y - 3.0),
                pos2(r.center().x + 3.0, r.center().y),
                pos2(r.center().x - 2.0, r.center().y + 3.0),
            ];
            painter.add(egui::Shape::convex_polygon(play, p.amber, Stroke::NONE));
        }
    }
}

/// Tarjeta modular estilo Consola Retro OS (Opción A) dentro de la pantalla CRT.
pub fn retro_card(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    icon: Option<SectionIcon>,
    title: &str,
    right_badge: Option<&str>,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    retro_card_custom_header(
        ui,
        p,
        icon,
        title,
        |ui| {
            if let Some(badge) = right_badge {
                ui.label(egui::RichText::new(badge).size(11.0).color(p.amber));
            }
        },
        add_contents,
    );
}

/// Variante de `retro_card` que permite colocar controles interactivos (p. ej. píldora clickeable)
/// en el extremo derecho del encabezado de la tarjeta.
pub fn retro_card_custom_header(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    icon: Option<SectionIcon>,
    title: &str,
    add_header_right: impl FnOnce(&mut egui::Ui),
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let frame = egui::Frame::new()
        .fill(p.crt_panel_bg)
        .stroke(Stroke::new(1.5, p.crt_panel_border))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(11, 8));

    frame.show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            if let Some(ic) = icon {
                let (irect, _) = ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
                draw_section_icon(ui.painter(), irect, p, ic);
            }
            ui.label(
                egui::RichText::new(title)
                    .strong()
                    .size(13.0)
                    .color(p.crt_text),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                add_header_right(ui);
            });
        });

        // Separador nítido retro
        let (sep_rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), 5.0), Sense::hover());
        ui.painter().line_segment(
            [
                pos2(sep_rect.left(), sep_rect.center().y),
                pos2(sep_rect.right(), sep_rect.center().y),
            ],
            Stroke::new(1.0, p.crt_widget_hover),
        );

        ui.add_space(2.0);
        add_contents(ui);
    });
}

/// Botón píldora táctil estilo consola retro (brilla en fósforo cuando está activo).
pub fn retro_pill(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    selected: bool,
    label: &str,
    min_size: Vec2,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(min_size, Sense::click());
    let painter = ui.painter();

    let bg = if selected {
        p.accent
    } else if resp.hovered() {
        p.crt_widget_hover
    } else {
        p.crt_widget_bg
    };

    let border = if selected {
        p.accent_bright
    } else if resp.hovered() {
        p.accent
    } else {
        lerp_color(p.crt_widget_hover, p.crt_panel_border, 0.35)
    };

    let text_color = if selected {
        p.accent_text
    } else {
        p.crt_text
    };

    if selected {
        // Sutil aura fósforo alrededor del botón activo
        let glow = Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 36);
        painter.rect_filled(rect.expand(1.5), 4.0, glow);
    }

    painter.rect_filled(rect, 3.0, bg);
    painter.rect_stroke(rect, 3.0, Stroke::new(1.2, border), StrokeKind::Inside);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::monospace(12.0),
        text_color,
    );

    if ui.is_enabled() {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Botón principal de guardado con indicador vectorial nítido (tilde vectorial al estar guardado,
/// punto ámbar cuando hay cambios pendientes).
pub fn retro_save_button(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    dirty: bool,
    min_size: Vec2,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(min_size, Sense::click());
    let painter = ui.painter();

    if dirty {
        let bg = if resp.hovered() {
            Color32::from_rgb(
                p.amber.r().saturating_add(16),
                p.amber.g().saturating_add(16),
                p.amber.b().saturating_add(16),
            )
        } else {
            p.amber
        };
        let glow = Color32::from_rgba_unmultiplied(p.amber.r(), p.amber.g(), p.amber.b(), 44);
        painter.rect_filled(rect.expand(1.5), 4.0, glow);
        painter.rect_filled(rect, 3.0, bg);
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.4, p.crt_text),
            StrokeKind::Inside,
        );

        let label = "GUARDAR CAMBIOS";
        let text_w = label.chars().count() as f32 * 7.2;
        let dot_r = 3.5;
        let gap = 7.0;
        let start_x = rect.center().x - (dot_r * 2.0 + gap + text_w) * 0.5;
        let cy = rect.center().y;
        painter.circle_filled(pos2(start_x + dot_r, cy), dot_r, p.accent_text);
        painter.text(
            pos2(start_x + dot_r * 2.0 + gap + text_w * 0.5, cy),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(12.0),
            p.accent_text,
        );
    } else {
        let bg = p.accent;
        let glow = Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 36);
        painter.rect_filled(rect.expand(1.5), 4.0, glow);
        painter.rect_filled(rect, 3.0, bg);
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.2, p.accent_bright),
            StrokeKind::Inside,
        );

        let label = "GUARDADO";
        let text_w = label.chars().count() as f32 * 7.2;
        let check_w = 11.0;
        let gap = 6.0;
        let start_x = rect.center().x - (text_w + gap + check_w) * 0.5;
        let cy = rect.center().y;

        painter.text(
            pos2(start_x + text_w * 0.5, cy),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(12.0),
            p.accent_text,
        );

        // Tilde vectorial nítida (evita depender de glifos Unicode en la fuente monoespaciada)
        let cx = start_x + text_w + gap;
        let stroke = Stroke::new(2.0, p.accent_text);
        painter.line_segment([pos2(cx, cy + 0.5), pos2(cx + 3.5, cy + 4.0)], stroke);
        painter.line_segment([pos2(cx + 3.5, cy + 4.0), pos2(cx + 10.5, cy - 4.0)], stroke);
    }

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Botón compacto Play/Pausa con icono vectorial nítido + texto corto para el reproductor de clips.
pub fn retro_transport_icon_pill(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    playing: bool,
    min_size: Vec2,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(min_size, Sense::click());
    let painter = ui.painter();

    let bg = if playing {
        if resp.hovered() {
            Color32::from_rgb(
                p.amber.r().saturating_add(16),
                p.amber.g().saturating_add(16),
                p.amber.b().saturating_add(16),
            )
        } else {
            p.amber
        }
    } else if resp.hovered() {
        p.crt_widget_hover
    } else {
        p.crt_widget_bg
    };
    let border = if playing {
        p.crt_text
    } else if resp.hovered() {
        p.accent
    } else {
        lerp_color(p.crt_widget_hover, p.crt_panel_border, 0.35)
    };

    painter.rect_filled(rect, 3.0, bg);
    painter.rect_stroke(rect, 3.0, Stroke::new(1.2, border), StrokeKind::Inside);

    let label = if playing { "PAUSA" } else { "PLAY" };
    let text_w = label.chars().count() as f32 * 7.0;
    let icon_w = 10.0;
    let gap = 5.0;
    let start_x = rect.center().x - (icon_w + gap + text_w) * 0.5;
    let cy = rect.center().y;

    if playing {
        let b_col = p.accent_text;
        painter.rect_filled(
            Rect::from_min_size(pos2(start_x + 1.0, cy - 5.0), vec2(3.0, 10.0)),
            0.8,
            b_col,
        );
        painter.rect_filled(
            Rect::from_min_size(pos2(start_x + 6.0, cy - 5.0), vec2(3.0, 10.0)),
            0.8,
            b_col,
        );
        painter.text(
            pos2(start_x + icon_w + gap + text_w * 0.5, cy),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(11.5),
            p.accent_text,
        );
    } else {
        let tri = vec![
            pos2(start_x + 1.0, cy - 5.0),
            pos2(start_x + icon_w, cy),
            pos2(start_x + 1.0, cy + 5.0),
        ];
        painter.add(egui::Shape::convex_polygon(tri, p.accent_bright, Stroke::NONE));
        painter.text(
            pos2(start_x + icon_w + gap + text_w * 0.5, cy),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(11.5),
            p.crt_text,
        );
    }

    if ui.is_enabled() {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Botón píldora con triángulo Play vectorial nítido (sin depender de glifos Unicode de la fuente).
pub fn retro_play_pill(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    label: &str,
    min_size: Vec2,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(min_size, Sense::click());
    let painter = ui.painter();

    let bg = if resp.hovered() {
        p.crt_widget_hover
    } else {
        p.crt_widget_bg
    };
    let border = if resp.hovered() {
        p.accent
    } else {
        lerp_color(p.crt_widget_hover, p.crt_panel_border, 0.35)
    };

    painter.rect_filled(rect, 3.0, bg);
    painter.rect_stroke(rect, 3.0, Stroke::new(1.2, border), StrokeKind::Inside);

    let text_w = label.chars().count() as f32 * 7.2;
    let icon_w = 9.0;
    let gap = 6.0;
    let start_x = rect.center().x - (icon_w + gap + text_w) * 0.5;
    let cy = rect.center().y;

    let tri = vec![
        pos2(start_x, cy - 5.0),
        pos2(start_x + icon_w, cy),
        pos2(start_x, cy + 5.0),
    ];
    painter.add(egui::Shape::convex_polygon(tri, p.accent_bright, Stroke::NONE));

    painter.text(
        pos2(start_x + icon_w + gap + text_w * 0.5, cy),
        Align2::CENTER_CENTER,
        label,
        FontId::monospace(12.0),
        p.crt_text,
    );

    if ui.is_enabled() {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Selector visual compacto de esquina PiP de cámara (dibuja una mini pantalla 16:9 con la esquina marcada).
pub fn draw_camera_corner_button(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    corner: CameraCorner,
    selected: bool,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 22.0), Sense::click());
    let painter = ui.painter();

    let bg = if selected {
        p.accent
    } else if resp.hovered() {
        p.crt_widget_hover
    } else {
        p.crt_widget_bg
    };
    let border = if selected {
        p.accent_bright
    } else if resp.hovered() {
        p.accent
    } else {
        lerp_color(p.crt_widget_hover, p.crt_panel_border, 0.35)
    };

    if selected {
        let glow = Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 36);
        painter.rect_filled(rect.expand(1.5), 4.0, glow);
    }
    painter.rect_filled(rect, 3.0, bg);
    painter.rect_stroke(rect, 3.0, Stroke::new(1.2, border), StrokeKind::Inside);

    // Mini marco de monitor 16:9 interior + recuadro PiP en la esquina seleccionada
    let frame_rect = rect.shrink2(vec2(5.0, 4.0));
    let frame_col = if selected { p.accent_text } else { p.crt_muted };
    painter.rect_stroke(frame_rect, 1.0, Stroke::new(1.0, frame_col), StrokeKind::Inside);

    let pip_size = vec2(7.0, 5.0);
    let pip_pos = match corner {
        CameraCorner::TopLeft => pos2(frame_rect.left() + 1.5, frame_rect.top() + 1.5),
        CameraCorner::TopRight => pos2(frame_rect.right() - pip_size.x - 1.5, frame_rect.top() + 1.5),
        CameraCorner::BottomLeft => pos2(frame_rect.left() + 1.5, frame_rect.bottom() - pip_size.y - 1.5),
        CameraCorner::BottomRight => pos2(
            frame_rect.right() - pip_size.x - 1.5,
            frame_rect.bottom() - pip_size.y - 1.5,
        ),
    };
    let pip_col = if selected { p.accent_text } else { p.accent };
    painter.rect_filled(Rect::from_min_size(pip_pos, pip_size), 0.8, pip_col);

    if ui.is_enabled() {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Checkbox retro cuadrado con marca nítida estilo consola.
pub fn retro_checkbox(ui: &mut egui::Ui, p: &RetroPalette, value: &mut bool, label: &str) -> Response {
    let mut changed = false;
    let resp = ui
        .horizontal(|ui| {
            let box_size = vec2(15.0, 15.0);
            let (rect, box_resp) = ui.allocate_exact_size(box_size, Sense::click());
            let lbl_resp = ui.add(
                egui::Label::new(egui::RichText::new(label).size(12.0).color(p.crt_text))
                    .sense(Sense::click()),
            );

            if box_resp.clicked() || lbl_resp.clicked() {
                *value = !*value;
                changed = true;
            }

            let hovered = box_resp.hovered() || lbl_resp.hovered();
            let painter = ui.painter();
            let border = if *value || hovered {
                p.accent
            } else {
                p.crt_panel_border
            };
            painter.rect_filled(rect, 2.0, p.crt_recess);
            painter.rect_stroke(rect, 2.0, Stroke::new(1.3, border), StrokeKind::Inside);

            if *value {
                painter.rect_filled(rect.shrink(3.5), 1.0, p.accent);
            }

            let combined = box_resp.union(lbl_resp);
            if ui.is_enabled() {
                combined.on_hover_cursor(egui::CursorIcon::PointingHand)
            } else {
                combined
            }
        })
        .inner;

    if changed {
        ui.ctx().request_repaint();
    }
    resp
}

/// Selector doble empotrado en el chasis superior estilo interruptor rocker CRT:
/// `[ AJUSTES ]  |  • [ CLIPS & DECK (N) ]`.
/// Devuelve `(settings_clicked, clips_clicked)`.
pub fn draw_hardware_tab_rocker(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    settings_active: bool,
    clips_count: usize,
) -> (bool, bool) {
    let left_lbl = "[ AJUSTES ]";
    let right_lbl = format!("[ CLIPS & DECK ({clips_count}) ]");
    let w_left = 114.0;
    let w_right = (right_lbl.chars().count() as f32 * 7.2 + 30.0).max(168.0);
    let height = 32.0;
    let pad = 3.0;
    let total_w = w_left + w_right + pad * 2.0 + 2.0;

    let (outer, _) = ui.allocate_exact_size(vec2(total_w, height), Sense::hover());
    let left_rect = Rect::from_min_size(
        pos2(outer.left() + pad, outer.top() + pad),
        vec2(w_left, height - pad * 2.0),
    );
    let right_rect = Rect::from_min_size(
        pos2(left_rect.right() + 2.0, outer.top() + pad),
        vec2(w_right, height - pad * 2.0),
    );

    let resp_left = ui
        .interact(
            left_rect,
            ui.scope_id().with("tab_settings_rocker"),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let resp_right = ui
        .interact(
            right_rect,
            ui.scope_id().with("tab_clips_rocker"),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    let painter = ui.painter();
    // Bahía empotrada oscura que aloja ambos interruptores
    draw_recessed_bay(painter, outer, p, 4.0);

    draw_rocker_half(
        painter,
        left_rect,
        p,
        settings_active,
        resp_left.hovered(),
        left_lbl,
    );
    draw_rocker_half(
        painter,
        right_rect,
        p,
        !settings_active,
        resp_right.hovered(),
        &right_lbl,
    );

    (resp_left.clicked(), resp_right.clicked())
}

fn draw_rocker_half(
    painter: &egui::Painter,
    rect: Rect,
    p: &RetroPalette,
    active: bool,
    hovered: bool,
    label: &str,
) {
    if active {
        // Pestaña activa: visor fósforo iluminado con LED encendido
        painter.rect_filled(rect, 2.5, p.crt_bg);
        painter.rect_stroke(
            rect,
            2.5,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), 150)),
            StrokeKind::Inside,
        );
        draw_status_led(
            painter,
            pos2(rect.left() + 10.0, rect.center().y),
            2.8,
            p.accent_bright,
            true,
            0.2,
        );
        painter.text(
            pos2(rect.center().x + 5.0, rect.center().y),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(11.5),
            p.accent_bright,
        );
    } else {
        // Pestaña inactiva: tecla física elevada del color del chasis
        let bg = if hovered {
            p.chassis_btn_hover
        } else {
            p.chassis_btn_bg
        };
        draw_beveled_box(
            painter,
            rect,
            bg,
            p.chassis_highlight,
            p.chassis_shadow,
            p.chassis_outline,
            2.5,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(11.5),
            p.chassis_text,
        );
    }
}

/// Dibuja un LED hardware de encendido montado en un aro/zócalo circular empotrado.
pub fn draw_power_led_socket(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    lit: bool,
    pulse_01: f32,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
    let painter = ui.painter();
    let c = rect.center();
    // Zócalo hundido en el chasis
    painter.circle_filled(c, 7.2, lerp_color(p.chassis_shadow, p.chassis_outline, 0.35));
    painter.circle_stroke(c, 7.2, Stroke::new(1.0, p.chassis_highlight));
    draw_status_led(
        painter,
        c,
        4.6,
        if lit { p.led_green } else { p.led_red },
        lit,
        pulse_01,
    );
    resp
}

/// Dibuja doble rejilla horizontal de parlante/ventilación CRT (2 columnas de 4 ranuras profundas).
pub fn draw_dual_vent_grille(ui: &mut egui::Ui, p: &RetroPalette, total_width: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(total_width, height), Sense::hover());
    let painter = ui.painter();
    let col_gap = 10.0;
    let col_w = ((rect.width() - col_gap) * 0.5).max(24.0);
    let rows = 4;
    let step = height / (rows as f32 + 0.6);
    let groove_col = lerp_color(p.chassis_shadow, p.chassis_outline, 0.55);

    for col in 0..2 {
        let x0 = rect.left() + col as f32 * (col_w + col_gap);
        let x1 = x0 + col_w;
        for i in 0..rows {
            let y = rect.top() + 3.0 + i as f32 * step;
            let slot_rect = Rect::from_min_max(pos2(x0, y), pos2(x1, y + 2.4));
            painter.rect_filled(slot_rect, 1.0, groove_col);
            painter.line_segment(
                [pos2(x0 + 0.5, y + 3.0), pos2(x1 - 0.5, y + 3.0)],
                Stroke::new(1.0, p.chassis_highlight),
            );
        }
    }
}

/// Placa de estado hardware moldeada en el chasis inferior derecho (LED + estado GPU/encoder).
pub fn draw_hardware_status_plate(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    daemon_running: bool,
    label: &str,
) {
    let width = (label.chars().count() as f32 * 6.8 + 32.0).clamp(160.0, 260.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
    let painter = ui.painter();

    // Placa biselada clara del color del chasis (como el prototipo CRT)
    draw_beveled_box(
        painter,
        rect,
        p.chassis_btn_bg,
        p.chassis_highlight,
        p.chassis_shadow,
        lerp_color(p.chassis_shadow, p.chassis_outline, 0.65),
        3.0,
    );
    draw_status_led(
        painter,
        pos2(rect.left() + 13.0, rect.center().y),
        4.2,
        if daemon_running { p.led_green } else { p.led_red },
        true,
        0.2,
    );
    painter.text(
        pos2(rect.left() + 24.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::monospace(10.8),
        p.chassis_text,
    );
}

/// Botón físico táctil del bisel hardware (ej. `[ \ ] GUARDAR CLIP` en rojo) con soporte deshabilitado.
pub fn hardware_action_button_enabled(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    label: &str,
    fill: Color32,
    text_color: Color32,
    height: f32,
    enabled: bool,
) -> Response {
    let width = (label.chars().count() as f32 * 7.4 + 20.0).max(34.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), sense);
    let painter = ui.painter();

    let (bg, txt_col, hi, sh, outline) = if !enabled {
        (
            lerp_color(p.chassis_btn_bg, p.chassis_bg, 0.55),
            p.chassis_muted,
            lerp_color(p.chassis_highlight, p.chassis_bg, 0.5),
            lerp_color(p.chassis_shadow, p.chassis_bg, 0.4),
            lerp_color(p.chassis_shadow, p.chassis_outline, 0.45),
        )
    } else {
        let bg = if resp.is_pointer_button_down_on() {
            fill.linear_multiply(0.85)
        } else if resp.hovered() {
            Color32::from_rgb(
                fill.r().saturating_add(16),
                fill.g().saturating_add(16),
                fill.b().saturating_add(16),
            )
        } else {
            fill
        };
        (
            bg,
            text_color,
            Color32::from_rgba_unmultiplied(255, 255, 255, 80),
            Color32::from_rgba_unmultiplied(0, 0, 0, 110),
            p.chassis_outline,
        )
    };

    draw_beveled_box(painter, rect, bg, hi, sh, outline, 3.0);

    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::monospace(11.5),
        txt_col,
    );

    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Botón físico dedicado de transporte para el Replay Buffer (Play / Pausa vectorial de alta nitidez).
pub fn hardware_transport_button(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    is_recording: bool,
    height: f32,
) -> Response {
    let width = 92.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let painter = ui.painter();

    let (base_fill, txt_col, hi, sh, border_col) = if is_recording {
        (
            p.amber,
            p.accent_text,
            Color32::from_rgba_unmultiplied(255, 255, 255, 105),
            Color32::from_rgba_unmultiplied(0, 0, 0, 115),
            p.chassis_outline,
        )
    } else {
        (
            p.chassis_btn_bg,
            p.chassis_text,
            p.chassis_highlight,
            p.chassis_shadow,
            if resp.hovered() {
                p.accent
            } else {
                p.chassis_outline
            },
        )
    };

    let bg = if resp.is_pointer_button_down_on() {
        base_fill.linear_multiply(0.86)
    } else if resp.hovered() {
        Color32::from_rgb(
            base_fill.r().saturating_add(16),
            base_fill.g().saturating_add(16),
            base_fill.b().saturating_add(16),
        )
    } else {
        base_fill
    };

    draw_beveled_box(painter, rect, bg, hi, sh, border_col, 3.0);

    let cy = rect.center().y;
    if is_recording {
        // Dos barras verticales nítidas de Pausa
        let bar_w = 3.5;
        let bar_h = 11.5;
        let x0 = rect.left() + 13.0;
        let b1 = Rect::from_min_size(pos2(x0, cy - bar_h * 0.5), vec2(bar_w, bar_h));
        let b2 = Rect::from_min_size(pos2(x0 + bar_w + 3.5, cy - bar_h * 0.5), vec2(bar_w, bar_h));
        painter.rect_filled(b1, 1.0, txt_col);
        painter.rect_filled(b2, 1.0, txt_col);

        let lbl_rect = Rect::from_min_max(pos2(x0 + bar_w * 2.0 + 7.5, rect.top()), rect.right_bottom());
        painter.text(
            lbl_rect.center(),
            Align2::CENTER_CENTER,
            "PAUSA",
            FontId::monospace(11.5),
            txt_col,
        );
    } else {
        // Triángulo Play vectorial verde fósforo centrado ópticamente
        let x0 = rect.left() + 13.0;
        let tri = vec![
            pos2(x0, cy - 5.8),
            pos2(x0 + 10.0, cy),
            pos2(x0, cy + 5.8),
        ];
        painter.add(egui::Shape::convex_polygon(
            tri,
            p.led_green,
            Stroke::new(1.0, p.chassis_outline),
        ));

        let lbl_rect = Rect::from_min_max(pos2(x0 + 13.0, rect.top()), rect.right_bottom());
        painter.text(
            lbl_rect.center(),
            Align2::CENTER_CENTER,
            "INICIAR",
            FontId::monospace(11.0),
            txt_col,
        );
    }

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Dibuja un LED hardware retro con halo y punto especular.
pub fn draw_status_led(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    color: Color32,
    lit: bool,
    pulse_01: f32,
) {
    if lit {
        let glow_alpha = (55.0 + pulse_01 * 55.0).clamp(30.0, 120.0) as u8;
        let glow = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), glow_alpha);
        painter.circle_filled(center, radius * 1.9, glow);
        painter.circle_filled(center, radius, color);
        painter.circle_filled(
            pos2(center.x - radius * 0.3, center.y - radius * 0.3),
            radius * 0.35,
            Color32::from_rgba_unmultiplied(255, 255, 255, 190),
        );
    } else {
        painter.circle_filled(center, radius, Color32::from_rgb(55, 58, 56));
    }
    painter.circle_stroke(
        center,
        radius,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 140)),
    );
}

/// Medidor VU LED segmentado retro (24 bloques continuos verde → ámbar → rojo).
pub fn draw_led_vu_meter(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    level: f32,
    segments: usize,
    height: f32,
    tooltip: &str,
) {
    let desired = vec2(ui.available_width(), height);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, 2.0, p.crt_recess);
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, p.crt_widget_hover),
        StrokeKind::Inside,
    );

    let inner = rect.shrink(2.0);
    let gap = 2.0;
    let total_gap = gap * (segments.saturating_sub(1)) as f32;
    let seg_w = ((inner.width() - total_gap) / segments as f32).max(2.0);

    let clamped = level.clamp(0.0, 1.0);
    let active_count = (clamped * segments as f32).round() as usize;

    for i in 0..segments {
        let frac = (i + 1) as f32 / segments as f32;
        let x = inner.left() + i as f32 * (seg_w + gap);
        let seg_rect = Rect::from_min_size(pos2(x, inner.top()), vec2(seg_w, inner.height()));

        let lit_color = if frac > 0.85 {
            p.led_red
        } else if frac > 0.65 {
            p.amber
        } else {
            p.accent
        };

        let color = if i < active_count {
            lit_color
        } else {
            Color32::from_rgba_unmultiplied(lit_color.r(), lit_color.g(), lit_color.b(), 28)
        };
        painter.rect_filled(seg_rect, 1.0, color);
    }

    resp.on_hover_text(format!("{tooltip}: {:.0}%", clamped * 100.0));
}

/// Tira LCD empotrada en la base central del chasis con mini-VU en vivo de Audio y Micrófono.
pub fn draw_mini_bezel_vu(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    game_lvl: f32,
    mic_lvl: f32,
    mic_enabled: bool,
) {
    let width = 266.0;
    let height = 26.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let painter = ui.painter();

    draw_recessed_bay(painter, rect, p, 3.5);

    let y_mid = rect.center().y;
    painter.text(
        pos2(rect.left() + 10.0, y_mid),
        Align2::LEFT_CENTER,
        "AUDIO",
        FontId::monospace(10.5),
        p.crt_text,
    );
    draw_mini_led_strip(
        painter,
        Rect::from_min_size(pos2(rect.left() + 50.0, rect.top() + 7.0), vec2(78.0, 12.0)),
        p,
        game_lvl,
        10,
    );

    painter.text(
        pos2(rect.left() + 140.0, y_mid),
        Align2::LEFT_CENTER,
        if mic_enabled { "MIC" } else { "OFF" },
        FontId::monospace(10.5),
        if mic_enabled { p.crt_text } else { p.danger },
    );
    draw_mini_led_strip(
        painter,
        Rect::from_min_size(pos2(rect.left() + 170.0, rect.top() + 7.0), vec2(84.0, 12.0)),
        p,
        if mic_enabled { mic_lvl } else { 0.0 },
        10,
    );
}

fn draw_mini_led_strip(
    painter: &egui::Painter,
    rect: Rect,
    p: &RetroPalette,
    level: f32,
    segments: usize,
) {
    let gap = 1.8;
    let seg_w = ((rect.width() - gap * (segments - 1) as f32) / segments as f32).max(2.0);
    let active = (level.clamp(0.0, 1.0) * segments as f32).round() as usize;
    for i in 0..segments {
        let frac = (i + 1) as f32 / segments as f32;
        let c = if frac > 0.85 {
            p.led_red
        } else if frac > 0.65 {
            p.amber
        } else {
            p.accent
        };
        let fill = if i < active {
            c
        } else {
            Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 32)
        };
        let x = rect.left() + i as f32 * (seg_w + gap);
        painter.rect_filled(
            Rect::from_min_size(pos2(x, rect.top()), vec2(seg_w, rect.height())),
            1.0,
            fill,
        );
    }
}

/// Ícono pixel-art procedural de disquete 3.5" para los clips del archivo.
pub fn draw_floppy_icon(painter: &egui::Painter, rect: Rect, p: &RetroPalette, selected: bool) {
    let body = if selected { p.accent } else { p.crt_widget_hover };
    let border = if selected { p.accent_bright } else { p.crt_muted };
    painter.rect_filled(rect, 2.0, body);
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, border), StrokeKind::Inside);

    // Obturador metálico superior
    let shutter = Rect::from_min_max(
        pos2(rect.left() + rect.width() * 0.25, rect.top() + 1.5),
        pos2(rect.right() - rect.width() * 0.22, rect.top() + rect.height() * 0.42),
    );
    painter.rect_filled(shutter, 1.0, p.crt_text);
    // Ranura oscura del obturador
    let slot = Rect::from_min_max(
        pos2(shutter.right() - 4.0, shutter.top() + 1.5),
        pos2(shutter.right() - 1.5, shutter.bottom() - 1.5),
    );
    painter.rect_filled(slot, 0.5, p.crt_recess);

    // Etiqueta blanca inferior del disquete
    let label = Rect::from_min_max(
        pos2(rect.left() + 3.0, rect.top() + rect.height() * 0.55),
        pos2(rect.right() - 3.0, rect.bottom() - 2.0),
    );
    painter.rect_filled(label, 1.0, p.crt_text);
    painter.line_segment(
        [
            pos2(label.left() + 2.0, label.center().y),
            pos2(label.right() - 2.0, label.center().y),
        ],
        Stroke::new(1.0, p.crt_recess),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimDragHandle {
    Start,
    End,
    Playhead,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimelineDragAction {
    TrimSeek(f64),
    PlayheadSeek(f64),
}

/// Barra de recorte interactiva con dos manijas arrastrables `[IN]` / `[OUT]` y cabezal de reproducción clickeable.
pub fn draw_dual_trim_timeline(
    ui: &mut egui::Ui,
    p: &RetroPalette,
    total_dur: f64,
    preview_time_secs: f64,
    trim_start: &mut f64,
    trim_end: &mut f64,
    drag_mode: &mut Option<TrimDragHandle>,
) -> Option<TimelineDragAction> {
    let mut action: Option<TimelineDragAction> = None;
    let total = total_dur.max(0.5);
    let min_gap = 0.5_f64.min(total);

    let desired = vec2(ui.available_width(), 44.0);
    let (outer_rect, resp) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let track_rect = Rect::from_min_max(
        pos2(outer_rect.left() + 8.0, outer_rect.top() + 18.0),
        pos2(outer_rect.right() - 8.0, outer_rect.bottom() - 6.0),
    );

    let frac_start = (*trim_start / total).clamp(0.0, 1.0) as f32;
    let frac_end = (*trim_end / total).clamp(0.0, 1.0) as f32;
    let x_start = track_rect.left() + track_rect.width() * frac_start;
    let x_end = track_rect.left() + track_rect.width() * frac_end;

    // Interacción con el mouse: si el puntero está cerca de [IN] o [OUT] (<= 14 px), mueve la manija;
    // si hace clic en otro punto de la barra, mueve el cabezal de reproducción sin alterar el recorte.
    if let Some(pointer) = resp.interact_pointer_pos() {
        if resp.drag_started() || resp.clicked() {
            let d_in = (pointer.x - x_start).abs();
            let d_out = (pointer.x - x_end).abs();
            let hit_thresh = 14.0;
            *drag_mode = Some(if d_in <= hit_thresh || d_out <= hit_thresh {
                if d_in <= d_out {
                    TrimDragHandle::Start
                } else {
                    TrimDragHandle::End
                }
            } else {
                TrimDragHandle::Playhead
            });
        }
        if resp.dragged() || resp.clicked() {
            let rel = ((pointer.x - track_rect.left()) / track_rect.width()).clamp(0.0, 1.0) as f64;
            let t = rel * total;
            match *drag_mode {
                Some(TrimDragHandle::Start) => {
                    *trim_start = t.clamp(0.0, (*trim_end - min_gap).max(0.0));
                    action = Some(TimelineDragAction::TrimSeek(*trim_start));
                }
                Some(TrimDragHandle::End) => {
                    *trim_end = t.clamp((*trim_start + min_gap).min(total), total);
                    action = Some(TimelineDragAction::TrimSeek(*trim_end));
                }
                Some(TrimDragHandle::Playhead) => {
                    action = Some(TimelineDragAction::PlayheadSeek(t.clamp(0.0, total)));
                }
                None => {}
            }
        }
    }
    if resp.drag_stopped() {
        *drag_mode = None;
    }

    // Recalcular posiciones tras interacción para pintar sin lag de 1 frame.
    let x0 = track_rect.left() + track_rect.width() * (*trim_start / total).clamp(0.0, 1.0) as f32;
    let x1 = track_rect.left() + track_rect.width() * (*trim_end / total).clamp(0.0, 1.0) as f32;

    let painter = ui.painter();

    // Carril base oscuro con marcas de regla retro
    painter.rect_filled(track_rect, 3.0, p.crt_recess);
    painter.rect_stroke(
        track_rect,
        3.0,
        Stroke::new(1.2, p.crt_widget_hover),
        StrokeKind::Inside,
    );
    for tick in 1..10 {
        let tx = track_rect.left() + track_rect.width() * (tick as f32 / 10.0);
        painter.line_segment(
            [pos2(tx, track_rect.top() + 3.0), pos2(tx, track_rect.bottom() - 3.0)],
            Stroke::new(1.0, p.crt_widget_bg),
        );
    }

    // Rango seleccionado en fósforo brillante
    let sel_rect = Rect::from_min_max(
        pos2(x0, track_rect.top() + 1.5),
        pos2(x1.max(x0 + 3.0), track_rect.bottom() - 1.5),
    );
    painter.rect_filled(sel_rect, 2.0, p.accent);

    // Aguja + cabezal triangular del timestamp actualmente visible / reproduciéndose
    let playhead_t = match action {
        Some(TimelineDragAction::TrimSeek(t)) | Some(TimelineDragAction::PlayheadSeek(t)) => t,
        None => preview_time_secs,
    };
    let preview_x =
        track_rect.left() + track_rect.width() * (playhead_t / total).clamp(0.0, 1.0) as f32;
    painter.line_segment(
        [
            pos2(preview_x, track_rect.top() - 2.0),
            pos2(preview_x, track_rect.bottom() + 2.0),
        ],
        Stroke::new(2.0, p.crt_text),
    );
    let head_tri = vec![
        pos2(preview_x - 4.5, track_rect.top() - 6.0),
        pos2(preview_x + 4.5, track_rect.top() - 6.0),
        pos2(preview_x, track_rect.top() - 0.5),
    ];
    painter.add(egui::Shape::convex_polygon(head_tri, p.crt_text, Stroke::NONE));

    // Evitar solapamiento de etiquetas [IN] y [OUT] cuando el rango es corto.
    let mut in_lbl_x = x0.clamp(track_rect.left() + 14.0, track_rect.right() - 24.0);
    let mut out_lbl_x = x1.clamp(track_rect.left() + 24.0, track_rect.right() - 16.0);
    let min_label_gap = 38.0;
    if out_lbl_x - in_lbl_x < min_label_gap {
        let mid = ((in_lbl_x + out_lbl_x) * 0.5).clamp(
            track_rect.left() + min_label_gap * 0.5 + 14.0,
            track_rect.right() - min_label_gap * 0.5 - 16.0,
        );
        in_lbl_x = mid - min_label_gap * 0.5;
        out_lbl_x = mid + min_label_gap * 0.5;
    }

    // Manija [IN]
    let in_active = *drag_mode == Some(TrimDragHandle::Start);
    draw_timeline_handle(painter, p, x0, in_lbl_x, track_rect, "IN", in_active);

    // Manija [OUT]
    let out_active = *drag_mode == Some(TrimDragHandle::End);
    draw_timeline_handle(painter, p, x1, out_lbl_x, track_rect, "OUT", out_active);

    resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    action
}

fn draw_timeline_handle(
    painter: &egui::Painter,
    p: &RetroPalette,
    x: f32,
    label_x: f32,
    track: Rect,
    tag: &str,
    active: bool,
) {
    let handle_rect = Rect::from_min_max(
        pos2(x - 4.5, track.top() - 2.0),
        pos2(x + 4.5, track.bottom() + 2.0),
    );
    let fill = if active { p.amber } else { p.crt_text };
    painter.rect_filled(handle_rect, 2.0, fill);
    painter.rect_stroke(
        handle_rect,
        2.0,
        Stroke::new(1.2, p.crt_recess),
        StrokeKind::Inside,
    );

    painter.text(
        pos2(label_x, track.top() - 10.0),
        Align2::CENTER_CENTER,
        format!("[{tag}]"),
        FontId::monospace(10.0),
        if active { p.amber } else { p.crt_muted },
    );
}
