//! Example demonstrating the WebView element.
//!
//! Run with:
//!   cargo run -p gpui --example webview --features webview

use gpui::{
    App, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb,
    size,
};

#[cfg(feature = "webview")]
use gpui::WebView;

const PHYSICS_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><style>
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body { background: #1e1e2e; overflow: hidden; }
    canvas { display: block; }
    #hud { position: fixed; top: 12px; left: 12px; color: #a6adc8; font: 12px system-ui;
           background: #181825cc; padding: 8px 12px; border-radius: 6px; }
    #hud b { color: #89b4fa; }
</style></head>
<body>
<canvas id="c"></canvas>
<div id="hud">
    <b>Click</b> to spawn particles &middot; <b>Hold</b> for stream &middot;
    Balls: <span id="count">0</span> &middot; FPS: <span id="fps">0</span>
</div>
<script>
const canvas = document.getElementById('c');
const ctx = canvas.getContext('2d');
let W, H;
function resize() { W = canvas.width = innerWidth; H = canvas.height = innerHeight; }
resize(); addEventListener('resize', resize);

const balls = [];
const gravity = 0.25;
const damping = 0.7;
const colors = ['#89b4fa','#f38ba8','#a6e3a1','#fab387','#cba6f7','#f9e2af','#94e2d5','#eba0ac'];

class Ball {
    constructor(x, y, vx, vy) {
        this.x = x; this.y = y;
        this.vx = vx + (Math.random()-0.5)*4;
        this.vy = vy + (Math.random()-0.5)*4;
        this.r = 4 + Math.random() * 12;
        this.color = colors[Math.floor(Math.random()*colors.length)];
        this.life = 1;
    }
    update() {
        this.vy += gravity;
        this.x += this.vx; this.y += this.vy;
        if (this.x - this.r < 0) { this.x = this.r; this.vx *= -damping; }
        if (this.x + this.r > W) { this.x = W - this.r; this.vx *= -damping; }
        if (this.y + this.r > H) { this.y = H - this.r; this.vy *= -damping; if (Math.abs(this.vy)<1) this.vy=0; }
        if (this.y - this.r < 0) { this.y = this.r; this.vy *= -damping; }
        this.life -= 0.001;
    }
    draw() {
        ctx.globalAlpha = Math.max(0, this.life);
        ctx.beginPath(); ctx.arc(this.x, this.y, this.r, 0, Math.PI*2);
        ctx.fillStyle = this.color; ctx.fill();
        ctx.globalAlpha = 1;
    }
}

let mouseDown = false, mx = W/2, my = H/2;
canvas.addEventListener('mousedown', e => { mouseDown = true; mx = e.clientX; my = e.clientY;
    for (let i=0;i<8;i++) balls.push(new Ball(mx,my,(Math.random()-0.5)*10,-Math.random()*8));
});
canvas.addEventListener('mousemove', e => { mx = e.clientX; my = e.clientY; });
canvas.addEventListener('mouseup', () => mouseDown = false);

let lastTime = performance.now(), frames = 0, fpsDisplay = 0;
function loop(now) {
    frames++;
    if (now - lastTime > 500) { fpsDisplay = Math.round(frames*1000/(now-lastTime)); frames=0; lastTime=now; }

    ctx.fillStyle = '#1e1e2e'; ctx.fillRect(0,0,W,H);

    if (mouseDown && balls.length < 2000) {
        for (let i=0;i<3;i++) balls.push(new Ball(mx,my,(Math.random()-0.5)*6,-Math.random()*6));
    }

    for (let i = balls.length-1; i >= 0; i--) {
        balls[i].update(); balls[i].draw();
        if (balls[i].life <= 0) balls.splice(i, 1);
    }

    document.getElementById('count').textContent = balls.length;
    document.getElementById('fps').textContent = fpsDisplay;
    requestAnimationFrame(loop);
}
requestAnimationFrame(loop);
</script>
</body>
</html>"#;

const COUNTER_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><style>
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body { background: #1e1e2e; color: #cdd6f4; font-family: system-ui, sans-serif;
           display: flex; flex-direction: column; align-items: center; justify-content: center;
           height: 100vh; }
    h1 { color: #89b4fa; margin-bottom: 12px; }
    p { color: #a6adc8; margin-bottom: 16px; }
    button { background: #89b4fa; color: #1e1e2e; border: none; padding: 8px 16px;
             border-radius: 6px; cursor: pointer; font-size: 14px; margin: 0 4px; }
    button:hover { background: #b4befe; }
    #counter { font-size: 64px; color: #f38ba8; margin: 20px 0; }
    .info { background: #313244; padding: 12px; border-radius: 8px; margin-top: 16px;
            font-size: 13px; max-width: 400px; text-align: center; }
    .info code { color: #a6e3a1; }
</style></head>
<body>
    <h1>Zed WebView</h1>
    <p>Interactive HTML + JavaScript running in WebKitGTK</p>
    <div id="counter">0</div>
    <div>
        <button onclick="count(-1)">-</button>
        <button onclick="count(1)">+</button>
        <button onclick="document.getElementById('counter').textContent='0'; n=0;">Reset</button>
    </div>
    <div class="info">
        User agent: <code id="ua"></code>
    </div>
    <script>
        let n = 0;
        function count(d) { n += d; document.getElementById('counter').textContent = n; }
        document.getElementById('ua').textContent = navigator.userAgent;
    </script>
</body>
</html>"#;

const PLOTLY_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<script src="https://cdn.plot.ly/plotly-2.35.2.min.js"></script>
<style>
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body { background: #1e1e2e; font-family: system-ui, sans-serif; }
    .container { padding: 16px; }
    h2 { color: #89b4fa; text-align: center; margin-bottom: 8px; }
    p { color: #a6adc8; text-align: center; font-size: 13px; margin-bottom: 12px; }
    #scatter, #bar, #surface { width: 100%; height: 350px; }
    .divider { height: 1px; background: #313244; margin: 16px 0; }
</style>
</head>
<body>
<div class="container">
    <h2>Interactive Plotly Charts</h2>
    <div id="scatter"></div>
    <div class="divider"></div>
    <div id="surface"></div>
</div>
<script>
    var layout = {
        paper_bgcolor: '#1e1e2e', plot_bgcolor: '#313244',
        font: { color: '#cdd6f4', size: 11 },
        margin: { t: 40, b: 40, l: 50, r: 20 },
        xaxis: { gridcolor: '#45475a' }, yaxis: { gridcolor: '#45475a' },
    };

    var x = Array.from({length: 50}, (_, i) => i * 0.2);
    Plotly.newPlot('scatter', [
        { x: x, y: x.map(v => Math.sin(v)), type: 'scatter', name: 'sin(x)',
          line: { color: '#89b4fa', width: 2 } },
        { x: x, y: x.map(v => Math.cos(v)), type: 'scatter', name: 'cos(x)',
          line: { color: '#f38ba8', width: 2 } },
        { x: x, y: x.map(v => Math.sin(v) * Math.cos(v * 0.5)), type: 'scatter',
          name: 'sin(x)*cos(x/2)', line: { color: '#a6e3a1', width: 2 } },
    ], { ...layout, title: { text: 'Trigonometric Functions', font: { size: 14 } } },
    { responsive: true });

    var size = 30;
    var z = [];
    for (var i = 0; i < size; i++) {
        z[i] = [];
        for (var j = 0; j < size; j++) {
            var x = (i - size/2) / 5, y = (j - size/2) / 5;
            z[i][j] = Math.sin(Math.sqrt(x*x + y*y)) * 5;
        }
    }
    Plotly.newPlot('surface', [{ z: z, type: 'surface',
        colorscale: [[0,'#89b4fa'],[0.5,'#1e1e2e'],[1,'#f38ba8']] }],
    { ...layout, title: { text: '3D Surface Plot', font: { size: 14 } },
      scene: { xaxis: { gridcolor: '#45475a' }, yaxis: { gridcolor: '#45475a' },
               zaxis: { gridcolor: '#45475a' },
               bgcolor: '#1e1e2e' },
      margin: { t: 40, b: 10, l: 10, r: 10 } },
    { responsive: true });
</script>
</body>
</html>"#;

struct WebViewExample;

impl Render for WebViewExample {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = WebView::active_count();

        div()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .text_xl()
                    .text_color(rgb(0x89b4fa))
                    .child("WebView Example"),
            )
            .child(
                div()
                    .text_color(rgb(0xa6adc8))
                    .child("Click a button to open a webview window"),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .id("btn-counter")
                            .px_4()
                            .py_2()
                            .bg(rgb(0x89b4fa))
                            .text_color(rgb(0x1e1e2e))
                            .rounded_md()
                            .cursor_pointer()
                            .child("Counter Demo")
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                WebView::open_html(COUNTER_HTML);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("btn-physics")
                            .px_4()
                            .py_2()
                            .bg(rgb(0xa6e3a1))
                            .text_color(rgb(0x1e1e2e))
                            .rounded_md()
                            .cursor_pointer()
                            .child("Physics Sandbox")
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                WebView::open_html(PHYSICS_HTML);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("btn-plotly")
                            .px_4()
                            .py_2()
                            .bg(rgb(0xf9e2af))
                            .text_color(rgb(0x1e1e2e))
                            .rounded_md()
                            .cursor_pointer()
                            .child("Plotly Charts")
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                WebView::open_html(PLOTLY_HTML);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("btn-website")
                            .px_4()
                            .py_2()
                            .bg(rgb(0xcba6f7))
                            .text_color(rgb(0x1e1e2e))
                            .rounded_md()
                            .cursor_pointer()
                            .child("zed.dev")
                            .on_click(cx.listener(|_this, _event, _window, cx| {
                                WebView::open_url("https://zed.dev");
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_color(rgb(0x585b70))
                    .text_sm()
                    .child(format!("{} webview(s) active", active)),
            )
    }
}

fn main() {
    #[cfg(not(feature = "webview"))]
    {
        eprintln!("WebView feature is not compiled in.");
        eprintln!("Run with: cargo run -p gpui --example webview --features webview");
        return;
    }

    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(500.), px(300.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| WebViewExample),
        )
        .unwrap();
        cx.activate(true);
    });
}
