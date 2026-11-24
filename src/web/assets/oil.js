// Oil on glass, behind the page: slicks of thin film on dark glass,
// banded light and dark in the theme accent, swirling slowly. The shader
// is neiam-org's assets/shaders/oil.frag (itself in neiam-device's
// Qt style), already rewritten for WebGL2 below; the colours come from
// the daisyUI theme variables, so it follows the theme picker. Under
// prefers-reduced-motion it holds one still frame.
(() => {
  const FRAG = "#version 300 es\nprecision highp float;\n// Oil on glass: slicks of thin film on dark glass, banded light and dark\n// as an oil film's interference is, in one colour, swirling slowly. The swirl is\n// domain-warped noise sampled at two points in its drift, crossfaded so\n// each is invisible as it jumps back: the shader holds no clock, only a\n// wrapped phase, as field.frag.\n\nin vec2 qt_TexCoord0;\nout vec4 fragColor;\n\nuniform mat4 qt_Matrix;\nuniform float qt_Opacity;\nuniform vec2 size;\nuniform float oilCycle;\nuniform float coverage;\nuniform float breath;\nuniform float grain;\nuniform float dither;\nuniform float seed;\nuniform vec4 base;\nuniform vec4 deep;\nuniform vec4 ink;\nuniform vec4 tone;\n\nconst float TAU = 6.2831853;\n\nfloat hash12(vec2 p)\n{\n    vec3 p3 = fract(vec3(p.xyx) * 0.1031);\n    p3 += dot(p3, p3.yzx + 33.33);\n    return fract((p3.x + p3.y) * p3.z);\n}\n\nfloat noise(vec2 p)\n{\n    vec2 i = floor(p);\n    vec2 f = fract(p);\n    vec2 u = f * f * (3.0 - 2.0 * f);\n    return mix(mix(hash12(i), hash12(i + vec2(1.0, 0.0)), u.x),\n               mix(hash12(i + vec2(0.0, 1.0)), hash12(i + vec2(1.0, 1.0)), u.x), u.y);\n}\n\nfloat fbm(vec2 p)\n{\n    float sum = 0.0;\n    float amp = 0.5;\n    mat2 turn = mat2(0.8, 0.6, -0.6, 0.8);\n    for (int i = 0; i < 5; i++) {\n        sum += amp * noise(p);\n        p = turn * p * 2.02 + 17.1;\n        amp *= 0.5;\n    }\n    return sum;\n}\n\n// The film's thickness, and how much film there is, at time t.\nvec2 film(vec2 p, float t)\n{\n    vec2 q = vec2(fbm(p + vec2(0.0, t)), fbm(p + vec2(5.2, -t) + 1.3));\n    vec2 r = vec2(fbm(p + 3.0 * q + vec2(1.7, 9.2) + 0.5 * t), fbm(p + 3.0 * q + vec2(8.3, 2.8) - 0.5 * t));\n    float thick = fbm(p + 2.5 * r);\n    float lo = mix(0.78, 0.3, coverage);\n    float slick = smoothstep(lo, lo + 0.14, fbm(p * 0.6 + 1.5 * q - 0.3 * t + 40.0));\n    return vec2(thick, slick);\n}\n\nvoid main()\n{\n    float px = 1.0 / min(size.x, size.y);\n    vec2 extent = size * px;\n    vec2 p = qt_TexCoord0 * extent;\n\n    float v = smoothstep(0.35, 1.25, length(p - 0.5 * extent));\n    vec3 colour = mix(base.rgb, deep.rgb, v);\n\n    // Two samples half a drift apart, each weighted to nothing as it wraps.\n    float a = oilCycle;\n    float b = fract(oilCycle + 0.5);\n    float w = abs(2.0 * a - 1.0);\n    vec2 s1 = film(p * 2.2, a * 6.0);\n    vec2 s2 = film(p * 2.2 + 31.7, b * 6.0);\n    vec2 f = mix(s1, s2, w);\n\n    // Interference bands, monochrome: the film goes light and dark as it\n    // thickens, in the theme's ink.\n    float th = f.x * 3.2;\n    float band = 0.5 + 0.5 * cos(TAU * th);\n    vec3 tint = mix(mix(deep.rgb, ink.rgb, 0.35), ink.rgb, band);\n    // Thin film at the slick's edge is brightest; its thick heart darker.\n    float edge = f.y * (1.0 - f.y) * 4.0;\n    float strength = f.y * (0.28 + 0.4 * edge) * breath;\n    colour = mix(colour, tint, strength);\n    colour = mix(colour, deep.rgb, f.y * smoothstep(0.62, 0.8, f.x) * 0.35);\n\n    // The glass: a faint sheen in a long diagonal.\n    float sheen = exp(-pow(dot(p - 0.5 * extent, vec2(0.6, 0.8)) / 0.35, 2.0));\n    colour += vec3(0.04) * sheen;\n\n    vec2 pixel = qt_TexCoord0 * size;\n    if (grain > 0.0)\n        colour += (hash12(pixel + seed * 1000.0) - 0.5) * 0.06 * grain;\n    if (dither > 0.0)\n        colour += (hash12(pixel + 0.5) + hash12(pixel + 91.3) - 1.0) / 255.0 * dither;\n\n    fragColor = vec4(colour, 1.0) * qt_Opacity;\n}\n";
  const VERT = "#version 300 es\nin vec2 position;\nout vec2 qt_TexCoord0;\nvoid main() {\n  qt_TexCoord0 = vec2(position.x * 0.5 + 0.5, 0.5 - position.y * 0.5);\n  gl_Position = vec4(position, 0.0, 1.0);\n}\n";
  const COVERAGE = 0.5;   // how much of the glass the slicks cover, 0-1
  const PERIOD = 240;     // seconds per drift cycle
  const FPS = 30;

  const start = () => {
    const canvas = document.createElement("canvas");
    canvas.id = "oil-slick";
    canvas.setAttribute("aria-hidden", "true");
    Object.assign(canvas.style, { position: "fixed", inset: "0", width: "100vw", height: "100vh", zIndex: "-1", pointerEvents: "none" });
    const gl = canvas.getContext("webgl2", { antialias: false, alpha: false });
    if (!gl) return;

    const compile = (type, text) => {
      const sh = gl.createShader(type);
      gl.shaderSource(sh, text);
      gl.compileShader(sh);
      if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(sh));
      return sh;
    };
    const program = gl.createProgram();
    try {
      gl.attachShader(program, compile(gl.VERTEX_SHADER, VERT));
      gl.attachShader(program, compile(gl.FRAGMENT_SHADER, FRAG));
    } catch (e) {
      console.error("oil slick:", e);
      return;
    }
    gl.bindAttribLocation(program, 0, "position");
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) return console.error("oil slick:", gl.getProgramInfoLog(program));
    gl.useProgram(program);
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    const loc = {};
    const u = name => (name in loc ? loc[name] : (loc[name] = gl.getUniformLocation(program, name)));

    document.body.prepend(canvas);
    // The page sits on the glass: the body lets the canvas show through.
    document.body.style.background = "transparent";

    // daisyUI keeps theme colours as "L C H" oklch components; let a 2D
    // canvas turn them into sRGB.
    const probe = document.createElement("canvas").getContext("2d", { willReadFrequently: true });
    const rgb = (name, fallback) => {
      const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
      probe.clearRect(0, 0, 1, 1);
      probe.fillStyle = fallback;
      if (v) probe.fillStyle = "oklch(" + v + ")";
      probe.fillRect(0, 0, 1, 1);
      const d = probe.getImageData(0, 0, 1, 1).data;
      return [d[0] / 255, d[1] / 255, d[2] / 255, 1];
    };
    let colours = null;
    const readColours = () => {
      colours = { base: rgb("--b1", "#651d1d"), deep: rgb("--b3", "#470005"), ink: rgb("--a", "#fef3c7"), tone: rgb("--p", "#b57979") };
    };
    readColours();
    new MutationObserver(() => { readColours(); kick(); }).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

    let clock = Math.random() * PERIOD;
    let frame = 0;
    let last = 0;
    let raf = 0;
    const still = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;

    const draw = dt => {
      const w = Math.max(1, Math.round(canvas.clientWidth));
      const h = Math.max(1, Math.round(canvas.clientHeight));
      if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; }
      clock += Math.min(dt, 0.25);
      frame = (frame + 1) % 100000;
      gl.viewport(0, 0, w, h);
      gl.uniform1f(u("qt_Opacity"), 1);
      gl.uniform2f(u("size"), w, h);
      gl.uniform1f(u("oilCycle"), (clock / PERIOD) % 1);
      gl.uniform1f(u("coverage"), COVERAGE);
      gl.uniform1f(u("breath"), 1);
      gl.uniform1f(u("grain"), 0);
      gl.uniform1f(u("dither"), 1);
      gl.uniform1f(u("seed"), (frame * 0.618034) % 1);
      for (const [k, c] of Object.entries(colours)) gl.uniform4f(u(k), c[0], c[1], c[2], c[3]);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    };
    const tick = now => {
      raf = 0;
      if (document.hidden) return;
      if (still()) return draw(0);
      if (!last || now - last >= 1000 / FPS - 2) {
        draw(last ? (now - last) / 1000 : 0);
        last = now;
      }
      raf = requestAnimationFrame(tick);
    };
    const kick = () => { if (!raf) raf = requestAnimationFrame(tick); };
    document.addEventListener("visibilitychange", () => { last = 0; kick(); });
    new ResizeObserver(kick).observe(canvas);
    kick();
  };

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();
})();
