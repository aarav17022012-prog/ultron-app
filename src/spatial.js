/* Spatial pictures (spatial.rs works out the depth): a picture drawn as a scene, each part shifted by how near it is,
   so it moves as a real view does when you lean to one side. Used by the lock screen (with its nearest part brought in
   front of the clock) and by the desktop wallpaper. One quad, one small shader, at most 30 frames a second, and none
   while it can't be seen. */
(function(){
const VS = 'attribute vec2 a; varying vec2 v; void main(){ v = a * .5 + .5; v.y = 1. - v.y; gl_Position = vec4(a, 0., 1.); }';
const FS = `precision highp float;
varying vec2 v; uniform sampler2D img, dep; uniform vec2 off, scale, shift; uniform float focus, fg, th;
void main(){
  vec2 p = shift + v * scale;
  // (where this pixel's ray meets the scene: a few steps, each looking up the depth where the last one landed)
  vec2 q = p;
  for (int i = 0; i < 6; i++){ float d = texture2D(dep, q).r; q = p + off * (d - focus); }
  vec4 c = texture2D(img, q);
  if (fg > .5){ float d = texture2D(dep, q).r; c.a = smoothstep(th - .045, th + .045, d); c.rgb *= c.a; }
  gl_FragColor = c;
}`;
function tex(gl, src){
  const t = gl.createTexture(); gl.bindTexture(gl.TEXTURE_2D, t);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR); gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE); gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, src);
  return t;
}
window.Spatial = function(canvas, {fg = false, strength = .03, zoom = 1.07} = {}){
  const gl = canvas.getContext('webgl', {premultipliedAlpha:true, alpha:fg, antialias:false, powerPreference:'low-power'});
  if (!gl) return null;
  const sh = (k, s) => { const o = gl.createShader(k); gl.shaderSource(o, s); gl.compileShader(o); return o; };
  const pr = gl.createProgram(); gl.attachShader(pr, sh(gl.VERTEX_SHADER, VS)); gl.attachShader(pr, sh(gl.FRAGMENT_SHADER, FS)); gl.linkProgram(pr); gl.useProgram(pr);
  const buf = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, buf); gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  const a = gl.getAttribLocation(pr, 'a'); gl.enableVertexAttribArray(a); gl.vertexAttribPointer(a, 2, gl.FLOAT, false, 0, 0);
  const U = n => gl.getUniformLocation(pr, n);
  gl.uniform1i(U('img'), 0); gl.uniform1i(U('dep'), 1); gl.uniform1f(U('fg'), fg ? 1 : 0); gl.uniform1f(U('focus'), .5);
  const S = {aim:[0, 0], cur:[0, 0], run:false, raf:0, last:0, iw:1, ih:1, th:.75, ready:false};
  function size(){
    const w = Math.round(canvas.clientWidth * devicePixelRatio), h = Math.round(canvas.clientHeight * devicePixelRatio);
    if (canvas.width !== w || canvas.height !== h){ canvas.width = w; canvas.height = h; }
    gl.viewport(0, 0, w, h);
    // (the picture covers the screen, a touch larger than it, so its edges never come into view as it moves)
    const ia = S.iw / S.ih, ca = w / Math.max(1, h);
    const sx = (ia > ca ? ca / ia : 1) / zoom, sy = (ia > ca ? 1 : ia / ca) / zoom;
    gl.uniform2f(U('scale'), sx, sy); gl.uniform2f(U('shift'), (1 - sx) / 2, (1 - sy) / 2);
    S.map = {sx, sy, ox:(1 - sx) / 2, oy:(1 - sy) / 2};
  }
  function frame(t){
    if (!S.run) return;
    S.raf = requestAnimationFrame(frame);
    if (t - S.last < 32) return;
    S.last = t;
    // (eased toward where it's aimed, with a slow drift of its own so it's alive when nothing moves)
    const dx = Math.sin(t * .00021) * .28, dy = Math.cos(t * .00016) * .18;
    S.cur[0] += (S.aim[0] + dx - S.cur[0]) * .07; S.cur[1] += (S.aim[1] + dy - S.cur[1]) * .07;
    gl.uniform2f(U('off'), S.cur[0] * strength, S.cur[1] * strength * .6);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
  return {
    S,
    set(img, depth, th){
      S.iw = img.naturalWidth || img.width; S.ih = img.naturalHeight || img.height;
      gl.activeTexture(gl.TEXTURE0); tex(gl, img); gl.activeTexture(gl.TEXTURE1); tex(gl, depth);
      if (th != null){ S.th = th; gl.uniform1f(U('th'), th); }
      S.ready = true; size();
    },
    resize: size,
    aim(x, y){ S.aim = [Math.max(-1, Math.min(1, x)), Math.max(-1, Math.min(1, y))]; },
    start(){ if (S.run || !S.ready) return; S.run = true; S.raf = requestAnimationFrame(frame); },
    stop(){ S.run = false; cancelAnimationFrame(S.raf); },
  };
};
// (a picture loaded from an address, ready to draw)
window.spatialPic = src => new Promise((ok, no) => { const i = new Image(); i.onload = () => ok(i); i.onerror = no; i.src = src; });
})();
