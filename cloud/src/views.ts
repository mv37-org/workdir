import { html, raw } from "hono/html";
import type { ApiKeyRow } from "./db";

// ─────────────────────────────────────────────────────────────────────────────
// workdir — control panel UI
// Design system: warm graphite + ember amber, IBM Plex Mono chrome,
// Schibsted Grotesk display. Hairline borders, square corners, corner ticks.
// ─────────────────────────────────────────────────────────────────────────────

const STYLES = `
  @font-face {
    font-family:"Geist Pixel";
    src:url("https://cdn.jsdelivr.net/gh/vercel/geist-font@v1.7.2/fonts/GeistPixel/webfonts/GeistPixel-Square.woff2") format("woff2");
    font-weight:400; font-style:normal; font-display:swap;
  }
  :root {
    --bg:#0a0908; --bg1:#0e0d0b; --bg2:#0c0b09;
    --line:#211f19; --line2:#2e2b22;
    --fg:#ede9de; --body:#b9b3a4; --muted:#8e8878; --faint:#5c5749;
    --amber:#ffb224; --amber2:#ffc95e; --ember:#ff7a1a;
    --ok:#8cd98c; --err:#ff6b5e;
    --sans:"Schibsted Grotesk",system-ui,-apple-system,sans-serif;
    --mono:"IBM Plex Mono",ui-monospace,"SF Mono",Menlo,monospace;
    --pixel:"Geist Pixel","IBM Plex Mono",ui-monospace,monospace;
  }
  * { box-sizing:border-box; }
  html { scrollbar-color:var(--line2) var(--bg); }
  body {
    margin:0; background:var(--bg); color:var(--fg);
    font-family:var(--sans); font-size:16px; line-height:1.6;
    -webkit-font-smoothing:antialiased; text-rendering:optimizeLegibility;
  }
  ::selection { background:var(--amber); color:#0a0908; }
  a { color:var(--amber); text-decoration:none; }
  a:hover { text-decoration:underline; text-underline-offset:3px; }
  code, pre { font-family:var(--mono); }
  .wrap { max-width:1100px; margin:0 auto; padding:0 24px; }

  /* ── nav ─────────────────────────────────────────────────────────────── */
  header.nav {
    position:sticky; top:0; z-index:50;
    background:rgba(10,9,8,.85); backdrop-filter:blur(12px);
    -webkit-backdrop-filter:blur(12px);
    border-bottom:1px solid var(--line);
  }
  .nav-in { display:flex; align-items:center; gap:28px; height:60px; }
  .logo {
    font:700 17px/1 var(--sans); letter-spacing:-.02em; color:var(--fg);
    display:flex; align-items:center;
  }
  .logo:hover { text-decoration:none; }
  .logo-cursor {
    width:8px; height:15px; background:var(--amber); margin-left:4px;
    animation:blink 1.4s steps(1) infinite;
  }
  .nav-links { display:flex; gap:22px; }
  .nav-links a {
    font:400 12.5px var(--mono); color:var(--muted); padding-bottom:2px;
    background:linear-gradient(var(--amber),var(--amber)) left bottom / 0 1px no-repeat;
    transition:background-size .22s ease, color .15s;
  }
  .nav-links a:hover { color:var(--fg); text-decoration:none; background-size:100% 1px; }
  .nav-cta { margin-left:auto; display:flex; gap:10px; align-items:center; }
  .nav-cta form { margin:0; }

  /* ── buttons & inputs ────────────────────────────────────────────────── */
  .btn {
    font:500 13px/1 var(--mono); letter-spacing:.01em;
    padding:11px 18px; border:1px solid var(--line2);
    background:transparent; color:var(--fg); cursor:pointer;
    display:inline-flex; align-items:center; gap:8px;
    transition:border-color .15s, background .15s, color .15s;
  }
  .btn:hover { border-color:var(--amber); color:var(--amber); text-decoration:none; }
  .btn.primary { background:var(--amber); border-color:var(--amber); color:#0a0908; font-weight:600; }
  .btn.primary:hover { background:var(--amber2); border-color:var(--amber2); color:#0a0908; }
  .btn.danger { color:var(--err); border-color:#3a2520; }
  .btn.danger:hover { border-color:var(--err); background:rgba(255,107,94,.08); color:var(--err); }
  .btn.sm { padding:8px 13px; font-size:12px; }
  .btn.block { width:100%; justify-content:center; }
  .btn .ar { display:inline-block; transition:transform .18s ease; }
  .btn:hover .ar { transform:translateX(3px); }
  .btn.primary:hover { box-shadow:0 0 26px rgba(255,178,36,.22); }

  label {
    display:block; font:500 11px var(--mono); letter-spacing:.14em;
    text-transform:uppercase; color:var(--muted); margin:18px 0 8px;
  }
  input[type=email], input[type=password], input[type=text] {
    width:100%; padding:12px 14px; background:var(--bg2);
    border:1px solid var(--line2); color:var(--fg);
    font:400 14px var(--mono); transition:border-color .15s, box-shadow .15s;
  }
  input::placeholder { color:var(--faint); }
  input:focus { outline:none; border-color:var(--amber); box-shadow:0 0 0 3px rgba(255,178,36,.12); }

  /* ── shared chrome ───────────────────────────────────────────────────── */
  .features > *, .code2 > *, .split > *, .stats > *, .tiles > *, .caps > * { min-width:0; }

  .corners { position:relative; }
  .corners::before, .corners::after {
    content:""; position:absolute; width:9px; height:9px;
    border:0 solid var(--amber); opacity:.9; pointer-events:none;
  }
  .corners::before { top:-1px; left:-1px; border-top-width:1px; border-left-width:1px; }
  .corners::after { bottom:-1px; right:-1px; border-bottom-width:1px; border-right-width:1px; }

  .kicker {
    font:500 11px var(--mono); letter-spacing:.18em; text-transform:uppercase;
    color:var(--muted); display:flex; align-items:center; gap:12px; margin-bottom:20px;
  }
  .kicker b { color:var(--amber); font-weight:600; }
  .kicker::after { content:""; height:1px; flex:1; background:var(--line); }

  section.block { padding:88px 0; border-top:1px solid var(--line); }
  .h2 { font:400 clamp(21px,2.9vw,31px)/1.25 var(--pixel); letter-spacing:.01em; margin:0 0 14px; }
  .lead { color:var(--body); font-size:16px; max-width:580px; margin:0; }

  .copy {
    font:500 11px var(--mono); color:var(--faint); background:none;
    border:1px solid var(--line2); padding:4px 11px; cursor:pointer;
    transition:color .15s, border-color .15s;
  }
  .copy:hover { color:var(--amber); border-color:var(--amber); }
  .copy.copied { color:var(--ok); border-color:var(--ok); }

  /* ── hero ────────────────────────────────────────────────────────────── */
  .hero { position:relative; padding:88px 0 0; overflow:hidden; }
  .hero-bg {
    position:absolute; inset:0; pointer-events:none;
    background-image:
      linear-gradient(rgba(237,233,222,.032) 1px, transparent 1px),
      linear-gradient(90deg, rgba(237,233,222,.032) 1px, transparent 1px);
    background-size:54px 54px;
    -webkit-mask-image:radial-gradient(ellipse 95% 85% at 50% 0%, #000 25%, transparent 72%);
    mask-image:radial-gradient(ellipse 95% 85% at 50% 0%, #000 25%, transparent 72%);
  }
  .hero-bg::after {
    content:""; position:absolute; inset:0;
    background:radial-gradient(640px 380px at 74% 36%, rgba(255,122,26,.075), transparent 70%);
  }
  .hero-bg::before {
    content:""; position:absolute; inset:0; opacity:.5;
    background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='180' height='180'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.9' numOctaves='2'/%3E%3CfeColorMatrix type='saturate' values='0'/%3E%3CfeComponentTransfer%3E%3CfeFuncA type='linear' slope='0.05'/%3E%3C/feComponentTransfer%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)'/%3E%3C/svg%3E");
  }
  .hero-grid {
    position:relative; display:grid; grid-template-columns:1.05fr .95fr;
    gap:56px; align-items:center;
  }
  .hero-grid > * { min-width:0; }
  .hero-tag { font:400 12.5px var(--mono); color:var(--muted); margin-bottom:22px; }
  .hero-tag b { color:var(--amber); font-weight:500; }
  h1.display {
    font:400 clamp(32px,4.3vw,50px)/1.14 var(--pixel);
    letter-spacing:.01em; margin:0 0 20px;
  }
  h1.display em { font-style:normal; color:var(--amber); }
  .hero p.sub { color:var(--body); font-size:16.5px; max-width:480px; margin:0 0 30px; }
  .hero-ctas { display:flex; gap:12px; flex-wrap:wrap; align-items:center; }
  .hero-foot { font:400 12px var(--mono); color:var(--faint); margin-top:26px; }
  .hero-foot a { color:var(--muted); }

  /* ── terminal ────────────────────────────────────────────────────────── */
  .term {
    background:var(--bg2); border:1px solid var(--line2);
    box-shadow:0 24px 80px rgba(0,0,0,.55), 0 0 90px rgba(255,178,36,.05);
    font:400 12.5px/1.9 var(--mono);
  }
  .term-bar {
    display:flex; align-items:center; gap:7px; padding:10px 14px;
    border-bottom:1px solid var(--line); color:var(--faint); font-size:11px;
  }
  .t-dot { width:9px; height:9px; border-radius:50%; background:#26241d; }
  .term-title { margin-left:8px; }
  .term-host { margin-left:auto; display:flex; align-items:center; gap:7px; }
  .term-host::before {
    content:""; width:6px; height:6px; border-radius:50%;
    background:var(--ok); box-shadow:0 0 8px rgba(140,217,140,.8);
  }
  .term-body { padding:18px 18px 14px; min-height:336px; overflow-x:auto; }
  .t-line, .t-out, .t-trace, .t-kv { opacity:0; animation:tIn .18s ease-out forwards; animation-delay:var(--d); }
  .t-p { color:var(--amber); margin-right:1ch; }
  .t-cmd {
    display:inline-block; overflow:hidden; white-space:pre; vertical-align:bottom;
    width:0; max-width:calc(100% - 2ch); animation-fill-mode:forwards; animation-delay:var(--d);
  }
  .t-cmd.c1 { animation-name:type1; animation-duration:.5s; animation-timing-function:steps(14,end); }
  .t-cmd.c2 { animation-name:type2; animation-duration:1.05s; animation-timing-function:steps(53,end); }
  .t-cmd.c3 { animation-name:type3; animation-duration:.55s; animation-timing-function:steps(24,end); }
  @keyframes type1 { to { width:14ch; } }
  @keyframes type2 { to { width:53ch; } }
  @keyframes type3 { to { width:24ch; } }
  @keyframes tIn { from { opacity:0; transform:translateY(3px); } to { opacity:1; transform:none; } }
  @keyframes blink { 0%,55% { opacity:1; } 56%,100% { opacity:0; } }
  .t-arrow { color:var(--ok); }
  .t-dim { color:var(--faint); }
  .t-em { color:var(--amber); }
  .t-kv { display:grid; grid-template-columns:11ch auto; padding-left:2ch; }
  .t-kv .k, .t-trace .k { color:var(--faint); }
  .t-trace { display:grid; grid-template-columns:11ch 6ch 1fr; align-items:center; padding-left:2ch; }
  .t-ms { color:var(--muted); }
  .t-bar {
    height:7px; width:var(--w); transform:scaleX(0); transform-origin:left;
    background:linear-gradient(90deg, var(--amber), var(--ember));
    box-shadow:0 0 10px rgba(255,140,26,.35);
    animation:grow .45s cubic-bezier(.2,.7,.2,1) forwards; animation-delay:var(--d);
  }
  @keyframes grow { to { transform:scaleX(1); } }
  .t-ready { color:var(--fg); font-weight:600; }
  .t-cursor {
    display:inline-block; width:.65ch; height:1.1em; background:var(--amber);
    vertical-align:text-bottom; animation:blink 1.1s steps(1) infinite;
  }

  /* ── stat strip ──────────────────────────────────────────────────────── */
  .stats {
    position:relative; display:grid; grid-template-columns:repeat(4,1fr);
    gap:1px; background:var(--line); border:1px solid var(--line); margin-top:78px;
  }
  .stat { background:var(--bg); padding:22px 24px; }
  .stat span {
    display:block; font:500 10.5px var(--mono); letter-spacing:.16em;
    text-transform:uppercase; color:var(--faint); margin-bottom:8px;
  }
  .stat b { font:400 24px/1 var(--pixel); color:var(--fg); }
  .stat b small { font-size:13px; color:var(--muted); font-weight:400; }
  .stat.hot b { color:var(--amber); }

  /* ── feature cards ───────────────────────────────────────────────────── */
  .features { display:grid; grid-template-columns:1fr 1fr; gap:1px; background:var(--line); border:1px solid var(--line); margin-top:44px; }
  .fcard { position:relative; background:var(--bg); padding:30px 28px 24px; transition:background .2s; }
  .fcard:hover { background:var(--bg1); }
  .fcard::after {
    content:"+"; position:absolute; top:10px; right:14px;
    font:400 14px var(--mono); color:var(--amber);
    opacity:0; transform:translateY(3px); transition:opacity .2s, transform .2s;
  }
  .fcard:hover::after { opacity:.9; transform:none; }
  .f-idx { font:600 11px var(--mono); letter-spacing:.18em; color:var(--amber); margin-bottom:42px; }
  .f-idx::after { content:" /"; color:var(--faint); }
  .fcard h3 { font:650 19px/1.3 var(--sans); letter-spacing:-.01em; margin:0 0 10px; }
  .fcard p { margin:0 0 24px; color:var(--body); font-size:14.5px; line-height:1.65; }
  .f-spec {
    font:400 11.5px var(--mono); color:var(--faint);
    border-top:1px dashed var(--line2); padding-top:12px;
    white-space:nowrap; overflow:hidden; text-overflow:ellipsis;
  }
  .f-spec em { color:var(--muted); font-style:normal; }

  /* ── code ────────────────────────────────────────────────────────────── */
  .code2 { display:grid; grid-template-columns:1fr 1fr; gap:24px; margin-top:44px; }
  .codebox { border:1px solid var(--line); background:var(--bg2); min-width:0; }
  .codehead {
    display:flex; justify-content:space-between; align-items:center; gap:12px;
    padding:9px 14px; border-bottom:1px solid var(--line);
    font:500 11.5px var(--mono); color:var(--muted);
  }
  .codehead .lang { color:var(--faint); }
  pre.code { margin:0; padding:18px; overflow-x:auto; font:400 12.5px/1.8 var(--mono); color:#c9c3b2; }
  .c-k { color:var(--amber); }
  .c-s { color:#9fcb8a; }
  .c-c { color:#565144; }
  .c-n { color:#ff9d5c; }
  .c-f { color:var(--fg); }

  /* ── capabilities ────────────────────────────────────────────────────── */
  .caps { border:1px solid var(--line); margin-top:44px; }
  .cap {
    display:grid; grid-template-columns:150px 1fr 330px; gap:24px;
    padding:19px 24px; border-top:1px solid var(--line); align-items:baseline;
    transition:background .15s, box-shadow .2s;
  }
  .cap:first-child { border-top:0; }
  .cap:hover { background:var(--bg1); box-shadow:inset 2px 0 0 var(--amber); }
  .cap-k { font:600 13px var(--mono); color:var(--amber); }
  .cap-d { margin:0; font-size:14px; color:var(--body); line-height:1.6; }
  .cap-c {
    font:400 11.5px var(--mono); color:var(--faint); text-align:right;
    white-space:nowrap; overflow:hidden; text-overflow:ellipsis;
  }

  /* ── architecture flow ───────────────────────────────────────────────── */
  .flow { margin-top:48px; display:flex; flex-direction:column; align-items:center; }
  .flow-node {
    width:min(560px,100%); border:1px solid var(--line2); background:var(--bg2);
    transition:border-color .2s;
  }
  .flow-node:hover { border-color:#3f3a2d; }
  .fn-head {
    display:flex; justify-content:space-between; align-items:baseline; gap:12px;
    padding:12px 16px; border-bottom:1px solid var(--line);
  }
  .fn-name { font:600 11.5px var(--mono); letter-spacing:.14em; text-transform:uppercase; color:var(--fg); }
  .fn-meta { font:400 11px var(--mono); color:var(--faint); }
  .fn-body { padding:11px 16px; font:400 12px/1.7 var(--mono); color:var(--muted); }
  .fn-body i { color:var(--faint); font-style:normal; margin:0 4px; }
  .flow-link {
    position:relative; height:58px; width:100%;
    display:flex; flex-direction:column; align-items:center;
  }
  .fl-line { position:relative; width:1px; height:100%; background:var(--line2); overflow:hidden; }
  .fl-line i {
    position:absolute; left:0; top:0; width:1px; height:14px;
    background:linear-gradient(180deg, transparent, var(--amber));
    animation:flowdrop 2.8s cubic-bezier(.55,.1,.45,.9) infinite; animation-delay:var(--fd,0s);
  }
  @keyframes flowdrop { 0% { top:-16px; } 100% { top:60px; } }
  .fl-tag {
    position:absolute; top:50%; left:calc(50% + 16px); transform:translateY(-50%);
    font:400 11px var(--mono); color:var(--faint); white-space:nowrap;
  }
  .fl-tag b { color:var(--amber); font-weight:500; }
  .flow-bin { position:relative; width:100%; display:flex; flex-direction:column; align-items:center; }
  .flow-bin::before {
    content:""; position:absolute; top:14px; bottom:14px; left:calc(50% - 322px);
    width:9px; border:1px solid var(--line2); border-right:0;
  }
  .bin-tag {
    position:absolute; top:50%; left:calc(50% - 348px);
    transform:translate(-50%,-50%) rotate(180deg); writing-mode:vertical-rl;
    font:500 10px var(--mono); letter-spacing:.24em; text-transform:uppercase; color:var(--amber);
    white-space:nowrap;
  }
  .flow-caption {
    margin-top:34px; font:400 11.5px var(--mono); letter-spacing:.08em; color:var(--faint);
    text-align:center;
  }
  .m-only { display:none; }

  /* ── deploy split ────────────────────────────────────────────────────── */
  .split { display:grid; grid-template-columns:1fr 1fr; gap:1px; background:var(--line); border:1px solid var(--line); margin-top:44px; }
  .splitcell { background:var(--bg); padding:34px 30px; display:flex; flex-direction:column; gap:0; }
  .splitcell .tag { font:600 11px var(--mono); letter-spacing:.18em; text-transform:uppercase; color:var(--amber); margin-bottom:20px; }
  .splitcell h3 { font:400 19px/1.3 var(--pixel); margin:0 0 10px; }
  .splitcell p { color:var(--body); font-size:14.5px; line-height:1.65; margin:0 0 24px; }
  .splitcell .grow { flex:1; }
  .install {
    display:flex; justify-content:space-between; align-items:center; gap:12px;
    border:1px dashed var(--line2); background:var(--bg2); padding:13px 16px;
  }
  .install code { font:400 12px var(--mono); color:var(--body); overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }

  /* ── cta band ────────────────────────────────────────────────────────── */
  .cta { text-align:center; padding:110px 0; border-top:1px solid var(--line); position:relative; overflow:hidden; }
  .cta::before {
    content:""; position:absolute; inset:0; pointer-events:none;
    background:radial-gradient(540px 300px at 50% 100%, rgba(255,122,26,.07), transparent 70%);
  }
  .cta .h2 { margin-bottom:10px; }
  .cta p { color:var(--body); margin:0 0 30px; }
  .cta-row { display:flex; gap:12px; justify-content:center; flex-wrap:wrap; }

  /* ── footer ──────────────────────────────────────────────────────────── */
  footer { border-top:1px solid var(--line); padding-top:56px; }
  .f-grid { display:grid; grid-template-columns:2fr 1fr 1fr 1fr; gap:32px; padding-bottom:64px; }
  .f-note { font:400 12.5px/1.8 var(--mono); color:var(--faint); max-width:300px; margin:0; }
  .f-note b { color:var(--muted); font-weight:500; }
  .f-col h4 { font:600 10.5px var(--mono); letter-spacing:.18em; text-transform:uppercase; color:var(--faint); margin:0 0 14px; }
  .f-col a { display:block; font:400 13px var(--mono); color:var(--muted); padding:4px 0; }
  .f-col a:hover { color:var(--amber); text-decoration:none; }
  .megamark { overflow:hidden; pointer-events:none; }
  .megamark div {
    font:400 clamp(70px,13.5vw,168px)/.92 var(--pixel); letter-spacing:.02em;
    text-align:center; color:#15130e;
    transform:translateY(16%); user-select:none;
  }

  /* ── auth ────────────────────────────────────────────────────────────── */
  .auth-wrap { max-width:400px; margin:9vh auto 90px; padding:0 24px; }
  .auth-card { border:1px solid var(--line2); background:var(--bg1); padding:34px 32px 30px; }
  .auth-card h1 { font:400 21px var(--pixel); margin:0 0 6px; }
  .auth-sub { font:400 12px var(--mono); color:var(--faint); margin:0 0 10px; }
  .auth-swap { text-align:center; font:400 12.5px var(--mono); color:var(--muted); margin-top:20px; }

  /* ── dashboard ───────────────────────────────────────────────────────── */
  .dash-head {
    display:flex; align-items:flex-end; justify-content:space-between;
    gap:16px; padding:44px 0 26px; flex-wrap:wrap;
  }
  .crumbs { font:400 12px var(--mono); color:var(--faint); }
  .crumbs b { color:var(--amber); font-weight:500; }
  .dash-head h1 { font:400 26px var(--pixel); margin:8px 0 0; }
  .chips { display:flex; gap:8px; flex-wrap:wrap; }
  .chip { font:400 11.5px var(--mono); color:var(--muted); border:1px solid var(--line); background:var(--bg1); padding:5px 10px; }

  .tiles { display:grid; grid-template-columns:repeat(3,1fr); gap:1px; background:var(--line); border:1px solid var(--line); margin:6px 0 8px; }
  .tile { background:var(--bg); padding:20px 22px; }
  .tile span { display:block; font:500 10.5px var(--mono); letter-spacing:.16em; text-transform:uppercase; color:var(--faint); margin-bottom:8px; }
  .tile b { font:600 25px/1 var(--mono); letter-spacing:-.02em; color:var(--fg); }
  .tile i { display:block; font:400 11px var(--mono); font-style:normal; color:var(--faint); margin-top:8px; }

  .panel { border:1px solid var(--line); background:var(--bg); margin:26px 0; }
  .panel-head {
    display:flex; justify-content:space-between; align-items:center; gap:14px;
    padding:13px 20px; border-bottom:1px solid var(--line); flex-wrap:wrap;
  }
  .panel-head h2 { font:600 12px var(--mono); letter-spacing:.14em; text-transform:uppercase; color:var(--fg); margin:0; }
  .panel-body { padding:20px; }
  .panel.newkey { border-color:rgba(255,178,36,.5); box-shadow:0 0 50px rgba(255,178,36,.05); }
  .keyrow { display:flex; gap:12px; align-items:stretch; }
  .keycode {
    flex:1; font:500 13.5px var(--mono); color:var(--amber2); background:var(--bg2);
    border:1px dashed var(--line2); padding:14px 16px; word-break:break-all;
  }
  .keynote { font:400 11.5px var(--mono); color:var(--faint); margin:10px 0 0; }

  .inline-form { display:flex; gap:10px; margin:0; align-items:center; }
  .inline-form input { width:210px; padding:8px 12px; font-size:12.5px; }

  table { width:100%; border-collapse:collapse; font:400 13px var(--mono); }
  th {
    text-align:left; padding:10px; font:500 10.5px var(--mono);
    letter-spacing:.16em; text-transform:uppercase; color:var(--faint);
    border-bottom:1px solid var(--line);
  }
  td { padding:12px 10px; border-bottom:1px solid var(--line); color:var(--body); }
  tr:last-child td { border-bottom:0; }
  tbody tr { transition:background .15s; }
  tbody tr:hover { background:var(--bg1); }
  td .name { color:var(--fg); }
  .dot { display:inline-block; width:7px; height:7px; border-radius:50%; margin-right:8px; vertical-align:1px; }
  .dot.on { background:var(--ok); box-shadow:0 0 6px rgba(140,217,140,.6); }
  .dot.off { background:var(--faint); }
  .status-on { color:var(--ok); } .status-off { color:var(--faint); }
  .empty {
    border:1px dashed var(--line2); padding:28px; text-align:center;
    font:400 12.5px var(--mono); color:var(--faint);
  }

  .flash { font:400 13px var(--mono); padding:12px 16px; border:1px solid; margin:16px 0; }
  .flash b { font-weight:600; margin-right:6px; }
  .flash.ok { border-color:rgba(140,217,140,.35); color:var(--ok); background:rgba(140,217,140,.05); }
  .flash.warn { border-color:rgba(255,178,36,.35); color:var(--amber); background:rgba(255,178,36,.05); }
  .flash.err { border-color:rgba(255,107,94,.35); color:var(--err); background:rgba(255,107,94,.06); }

  .muted { color:var(--muted); } .small { font-size:13px; }

  /* ── scroll reveal ───────────────────────────────────────────────────── */
  .js .rev {
    opacity:0; transform:translateY(16px);
    transition:opacity .55s ease, transform .55s cubic-bezier(.2,.7,.2,1);
    transition-delay:var(--rd,0s);
  }
  .js .rev.vis { opacity:1; transform:none; }
  .features .rev:nth-child(2) { --rd:.07s; } .features .rev:nth-child(3) { --rd:.14s; }
  .features .rev:nth-child(4) { --rd:.21s; }
  .code2 .rev:nth-child(2), .split .rev:nth-child(2) { --rd:.1s; }
  .caps .rev:nth-child(2) { --rd:.05s; } .caps .rev:nth-child(3) { --rd:.1s; }
  .caps .rev:nth-child(4) { --rd:.15s; } .caps .rev:nth-child(5) { --rd:.2s; }
  .caps .rev:nth-child(6) { --rd:.25s; }

  .replay {
    font:500 10.5px var(--mono); letter-spacing:.08em; color:var(--faint);
    background:none; border:0; cursor:pointer; padding:2px 0 2px 12px;
    transition:color .15s;
  }
  .replay:hover { color:var(--amber); }
  .replay::before { content:"↺ "; }

  /* ── responsive ──────────────────────────────────────────────────────── */
  @media (max-width:920px) {
    .hero-grid { grid-template-columns:1fr; gap:40px; }
    .features, .split, .code2 { grid-template-columns:1fr; }
    .stats { grid-template-columns:1fr 1fr; }
    .cap { grid-template-columns:120px 1fr; }
    .cap-c { display:none; }
    .f-grid { grid-template-columns:1fr 1fr; }
    .tiles { grid-template-columns:1fr; }
    .nav-links { display:none; }
    .flow-bin::before, .bin-tag { display:none; }
    .m-only { display:inline; }
    .fl-tag { left:calc(50% + 12px); }
  }
  @media (max-width:560px) {
    .stats { grid-template-columns:1fr; }
    .f-grid { grid-template-columns:1fr; }
    .keyrow { flex-direction:column; }
    .panel-head { align-items:flex-start; }
  }

  @media (prefers-reduced-motion:reduce) {
    *, *::before, *::after { animation:none !important; transition:none !important; }
    .t-line, .t-out, .t-trace, .t-kv { opacity:1; }
    .t-cmd.c1 { width:14ch; } .t-cmd.c2 { width:53ch; } .t-cmd.c3 { width:24ch; }
    .t-bar { transform:none; }
    .js .rev { opacity:1 !important; transform:none !important; }
    .fl-line i { display:none; }
  }
`;

const SCRIPT = `
  document.querySelectorAll("[data-copy]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var el = document.querySelector(btn.getAttribute("data-copy"));
      if (!el) return;
      navigator.clipboard.writeText(el.innerText.trim()).then(function () {
        btn.classList.add("copied");
        var prev = btn.textContent;
        btn.textContent = "copied";
        setTimeout(function () { btn.classList.remove("copied"); btn.textContent = prev; }, 1500);
      });
    });
  });

  var replay = document.querySelector("[data-replay]");
  if (replay) {
    replay.addEventListener("click", function () {
      var body = document.querySelector(".term-body");
      if (body) body.replaceWith(body.cloneNode(true));
    });
  }

  if ("IntersectionObserver" in window) {
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) { e.target.classList.add("vis"); io.unobserve(e.target); }
      });
    }, { threshold: 0.12, rootMargin: "0px 0px -8% 0px" });
    document.querySelectorAll(".rev").forEach(function (el) { io.observe(el); });
  } else {
    document.querySelectorAll(".rev").forEach(function (el) { el.classList.add("vis"); });
  }
`;

const FAVICON =
  "data:image/svg+xml," +
  encodeURIComponent(
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" fill="#0a0908"/><rect x="5" y="19" width="13" height="7" fill="#ffb224"/><rect x="22" y="19" width="5" height="7" fill="#3a3526"/></svg>`,
  );

// Hand-highlighted code samples (trusted markup, rendered via raw()).
const TS_CODE = `<span class="c-k">import</span> { Client } <span class="c-k">from</span> <span class="c-s">"@workdir/sdk"</span>;

<span class="c-k">const</span> wd = <span class="c-k">new</span> <span class="c-f">Client</span>(API, process.env.<span class="c-f">WORKDIR_KEY</span>);

<span class="c-k">const</span> box = <span class="c-k">await</span> wd.sandboxes.<span class="c-f">create</span>({
  resources: { cpu: <span class="c-n">2</span>, memoryMb: <span class="c-n">4096</span> },
  startup: {
    git:      { url: <span class="c-s">"github.com/acme/app"</span> },
    commands: [{ run: <span class="c-s">"pnpm dev"</span>, background: <span class="c-n">true</span> }],
    ports:    [<span class="c-n">3000</span>],
  },
});

box.urls.ports[<span class="c-s">"3000"</span>];
<span class="c-c">// → https://3000-sb_9f3ka2.sandboxes.workdir.dev</span>`;

const PY_CODE = `<span class="c-k">from</span> workdir <span class="c-k">import</span> Client

wd = <span class="c-f">Client</span>(<span class="c-s">"https://api.workdir.dev"</span>, api_key=KEY)

box = wd.sandboxes.<span class="c-f">create</span>()              <span class="c-c"># ready in ~40ms</span>
out = box.<span class="c-f">exec</span>(<span class="c-s">"python3 -c 'print(2+2)'"</span>)
<span class="c-f">print</span>(out.stdout)                        <span class="c-c"># 4</span>

box.<span class="c-f">delete</span>()                             <span class="c-c"># billed: 3 seconds</span>`;

const QUICKSTART = `<span class="c-c"># point any HTTP client at the API with your key</span>
<span class="c-k">export</span> WORKDIR_API_URL=<span class="c-s">https://api.workdir.dev</span>
<span class="c-k">export</span> WORKDIR_KEY=<span class="c-s">&lt;your key above&gt;</span>

curl -s -X POST <span class="c-f">$WORKDIR_API_URL</span>/v1/sandboxes \\
  -H <span class="c-s">"Authorization: Bearer <span class="c-f">$WORKDIR_KEY</span>"</span>`;

// ─────────────────────────────────────────────────────────────────────────────

function layout(
  title: string,
  body: ReturnType<typeof html>,
  opts?: { user?: { email: string }; description?: string },
) {
  const description =
    opts?.description ??
    "Run untrusted code in Firecracker microVMs that boot in ~40ms. One API for AI agents, CI jobs, and app previews — billed per second. Open source.";
  return html`<!doctype html>
    <html lang="en">
      <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <meta name="description" content="${description}" />
        <meta name="theme-color" content="#0a0908" />
        <meta property="og:title" content="${title}" />
        <meta property="og:description" content="${description}" />
        <title>${title}</title>
        <script>
          document.documentElement.classList.add("js");
        </script>
        <link rel="icon" href="${FAVICON}" />
        <link rel="preconnect" href="https://fonts.googleapis.com" />
        <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin />
        <link
          href="https://fonts.googleapis.com/css2?family=Schibsted+Grotesk:ital,wght@0,400..900;1,400..900&family=IBM+Plex+Mono:ital,wght@0,400;0,500;0,600;1,400&display=swap"
          rel="stylesheet"
        />
        <style>
          ${raw(STYLES)}
        </style>
      </head>
      <body>
        <header class="nav">
          <div class="wrap nav-in">
            <a class="logo" href="/">workdir<span class="logo-cursor"></span></a>
            <nav class="nav-links">
              <a href="https://github.com/mv37-org/workdir/blob/main/docs/API.md">docs</a>
              <a href="https://github.com/mv37-org/workdir/blob/main/docs/FEATURES.md">features</a>
              <a href="https://github.com/mv37-org/workdir">github</a>
            </nav>
            <div class="nav-cta">
              ${opts?.user
                ? html`<a class="btn sm" href="/dashboard">dashboard</a>
                    <form method="post" action="/logout">
                      <button class="btn sm" type="submit">log out</button>
                    </form>`
                : html`<a class="btn sm" href="/login">log in</a>
                    <a class="btn sm primary" href="/signup">get api key <span class="ar">→</span></a>`}
            </div>
          </div>
        </header>
        ${body}
        <footer>
          <div class="wrap">
            <div class="f-grid">
              <div>
                <p class="f-note">
                  <b>workdir</b> — disposable computers for software that writes software.
                  open source under AGPL-3.0.
                </p>
              </div>
              <div class="f-col">
                <h4>product</h4>
                <a href="/signup">get an api key</a>
                <a href="/dashboard">dashboard</a>
                <a href="/healthz">status</a>
              </div>
              <div class="f-col">
                <h4>docs</h4>
                <a href="https://github.com/mv37-org/workdir/blob/main/docs/API.md">api reference</a>
                <a href="https://github.com/mv37-org/workdir/blob/main/docs/FEATURES.md">features</a>
                <a href="https://github.com/mv37-org/workdir/blob/main/docs/ARCHITECTURE.md">architecture</a>
                <a href="https://github.com/mv37-org/workdir/blob/main/docs/DEPLOY.md">self-hosting</a>
              </div>
              <div class="f-col">
                <h4>open source</h4>
                <a href="https://github.com/mv37-org/workdir">github</a>
                <a href="https://github.com/mv37-org/workdir/blob/main/LICENSE">license · AGPL-3.0</a>
                <a href="https://firecracker-microvm.github.io/">firecracker</a>
              </div>
            </div>
          </div>
          <div class="megamark" aria-hidden="true"><div>workdir</div></div>
        </footer>
        <script>
          ${raw(SCRIPT)}
        </script>
      </body>
    </html>`;
}

// ─────────────────────────────────────────────────────────────────────────────
// Landing
// ─────────────────────────────────────────────────────────────────────────────

function heroTerminal() {
  return html`<div
    class="term corners"
    role="img"
    aria-label="Terminal demo: workdir create boots a sandbox via the hot pool in 38 milliseconds, runs a command, and deletes it for $0.00003."
  >
    <div class="term-bar">
      <span class="t-dot"></span><span class="t-dot"></span><span class="t-dot"></span>
      <span class="term-title">workdir — zsh</span>
      <span class="term-host">api.workdir.dev</span>
      <button class="replay" data-replay type="button" aria-label="replay the demo">replay</button>
    </div>
    <div class="term-body" aria-hidden="true">
      <div class="t-line" style="--d:.35s"><span class="t-p">$</span><span class="t-cmd c1" style="--d:.45s">workdir create</span></div>
      <div class="t-out" style="--d:1.05s"><span class="t-arrow">→</span> sandbox <span class="t-em">sb_9f3ka2</span> created</div>
      <div class="t-kv" style="--d:1.2s"><span class="k">boot_path</span><span class="t-em">hot_pool</span></div>
      <div class="t-trace" style="--d:1.34s"><span class="k">queue</span><span class="t-ms">2ms</span><i class="t-bar" style="--w:10px;--d:1.4s"></i></div>
      <div class="t-trace" style="--d:1.46s"><span class="k">assign</span><span class="t-ms">4ms</span><i class="t-bar" style="--w:20px;--d:1.52s"></i></div>
      <div class="t-trace" style="--d:1.58s"><span class="k">kernel</span><span class="t-ms">19ms</span><i class="t-bar" style="--w:95px;--d:1.64s"></i></div>
      <div class="t-trace" style="--d:1.7s"><span class="k">agent</span><span class="t-ms">13ms</span><i class="t-bar" style="--w:65px;--d:1.76s"></i></div>
      <div class="t-kv" style="--d:1.95s"><span class="k">ready</span><span><span class="t-ready">38ms</span><span class="t-dim"> ── total</span></span></div>
      <div class="t-line" style="--d:2.35s"><span class="t-p">$</span><span class="t-cmd c2" style="--d:2.45s">workdir exec sb_9f3ka2 -- echo "hello from a microVM"</span></div>
      <div class="t-out" style="--d:3.6s">hello from a microVM</div>
      <div class="t-line" style="--d:3.95s"><span class="t-p">$</span><span class="t-cmd c3" style="--d:4.05s">workdir delete sb_9f3ka2</span></div>
      <div class="t-out" style="--d:4.75s"><span class="t-arrow">→</span> deleted · ran 11s · billed <span class="t-em">$0.00003</span></div>
      <div class="t-line" style="--d:5s"><span class="t-p">$</span><span class="t-cursor"></span></div>
    </div>
  </div>`;
}

export function landingPage(user?: { email: string }) {
  return layout(
    "workdir — Firecracker microVM sandboxes for AI agents, CI, and previews",
    html`
      <section class="hero">
        <div class="hero-bg"></div>
        <div class="wrap">
          <div class="hero-grid">
            <div>
              <div class="hero-tag"><b>$</b> sandboxes for ai agents · open source · billed by the second</div>
              <h1 class="display">A computer for <em>every agent</em>.</h1>
              <p class="sub">
                workdir hands your agent a real Linux machine — booted in ~40&nbsp;ms, sealed off from
                everything you care about, gone when the job is done. You pay for the seconds in
                between.
              </p>
              <div class="hero-ctas">
                <a class="btn primary" href="/signup">get an api key <span class="ar">→</span></a>
                <a class="btn" href="https://github.com/mv37-org/workdir">self-host it</a>
              </div>
              <div class="hero-foot">
                npm i @workdir/sdk · pip install workdir ·
                <a href="https://github.com/mv37-org/workdir/blob/main/docs/API.md">or just curl</a>
              </div>
            </div>
            ${heroTerminal()}
          </div>

          <div class="stats corners">
            <div class="stat hot"><span>p50 boot, hot pool</span><b>38<small>ms</small></b></div>
            <div class="stat"><span>base shape, 1 vCPU / 2 GB</span><b>$0.009<small>/hr</small></b></div>
            <div class="stat"><span>billing granularity</span><b>1<small>s</small></b></div>
            <div class="stat"><span>license</span><b>AGPL<small>-3.0</small></b></div>
          </div>
        </div>
      </section>

      <section class="block">
        <div class="wrap">
          <div class="kicker"><b>//</b> why workdir</div>
          <h2 class="h2">A real computer, priced like a function call.</h2>
          <p class="lead">
            Not a shared container. Not a 30-second VM queue. Every sandbox is its own machine — and
            it costs less than the API call that asked for it.
          </p>
          <div class="features">
            <div class="fcard rev">
              <div class="f-idx">01</div>
              <h3>No waiting room</h3>
              <p>
                Sandboxes come from warm pools, ready before your agent finishes its sentence. Forty
                milliseconds, most days.
              </p>
              <div class="f-spec">boot_path: <em>hot_pool</em> · p50 &lt;50ms to first command</div>
            </div>
            <div class="fcard rev">
              <div class="f-idx">02</div>
              <h3>Disposable on purpose</h3>
              <p>
                Spin up a fleet for one task and delete it mid-thought. The meter stops the second
                you do.
              </p>
              <div class="f-spec">$0.009 / sandbox-hour · <em>1 vCPU / 2 GB</em> base shape</div>
            </div>
            <div class="fcard rev">
              <div class="f-idx">03</div>
              <h3>Run code you didn't write</h3>
              <p>
                Each sandbox gets its own kernel behind a hardware boundary. Your agent's experiments
                stay in the blast room.
              </p>
              <div class="f-spec">firecracker microVM · jailer · <em>own kernel</em></div>
            </div>
            <div class="fcard rev">
              <div class="f-idx">04</div>
              <h3>Honest numbers</h3>
              <p>
                Every create reports how it booted and how long each step took. Cold starts don't
                hide in our p50 — or yours.
              </p>
              <div class="f-spec">{ boot_path, timings_ms } <em>on every create</em></div>
            </div>
          </div>
        </div>
      </section>

      <section class="block">
        <div class="wrap">
          <div class="kicker"><b>//</b> the api</div>
          <h2 class="h2">Three calls. Create, exec, delete.</h2>
          <p class="lead">
            Typed SDKs for TypeScript and Python, or curl if that's more your thing. Clone a repo,
            run commands, open a port — get back a URL you can share.
          </p>
          <div class="code2">
            <div class="codebox rev">
              <div class="codehead">
                <span>agent.ts</span><span class="lang">typescript</span>
                <button class="copy" data-copy="#code-ts" type="button">copy</button>
              </div>
              <pre class="code" id="code-ts">${raw(TS_CODE)}</pre>
            </div>
            <div class="codebox rev">
              <div class="codehead">
                <span>ci.py</span><span class="lang">python</span>
                <button class="copy" data-copy="#code-py" type="button">copy</button>
              </div>
              <pre class="code" id="code-py">${raw(PY_CODE)}</pre>
            </div>
          </div>
        </div>
      </section>

      <section class="block">
        <div class="wrap">
          <div class="kicker"><b>//</b> batteries included</div>
          <h2 class="h2">Everything an agent reaches for.</h2>
          <p class="lead">
            Six flags on <code>create()</code>. None of them on the meter until you flip one.
          </p>
          <div class="caps corners">
            <div class="cap rev">
              <code class="cap-k">secrets</code>
              <p class="cap-d">
                Encrypted at rest, injected at runtime, never written to disk or snapshots. Your keys
                outlive nothing.
              </p>
              <code class="cap-c">startup: { secrets: ["OPENAI_API_KEY"] }</code>
            </div>
            <div class="cap rev">
              <code class="cap-k">docker</code>
              <p class="cap-d">
                Containers inside the sandbox, isolated with everything else. The host never meets
                your Dockerfile.
              </p>
              <code class="cap-c">docker: { enabled: true }</code>
            </div>
            <div class="cap rev">
              <code class="cap-k">mounts.s3</code>
              <p class="cap-d">
                Mount a bucket like a folder — S3, R2, or MinIO — with credentials pulled from your
                encrypted secrets.
              </p>
              <code class="cap-c">{ type: "s3", bucket: "data", mount_path: "/mnt/data" }</code>
            </div>
            <div class="cap rev">
              <code class="cap-k">browser</code>
              <p class="cap-d">
                Chromium with Playwright wired up, a VNC view to watch your agent browse, CDP if
                you'd rather drive.
              </p>
              <code class="cap-c">image: "browser" → urls.vnc · urls.cdp</code>
            </div>
            <div class="cap rev">
              <code class="cap-k">previews</code>
              <p class="cap-d">
                Open a port, get a public HTTPS URL. Demo the app your agent just built without
                deploying it.
              </p>
              <code class="cap-c">ports: [3000] → 3000-sb_x.sandboxes.workdir.dev</code>
            </div>
            <div class="cap rev">
              <code class="cap-k">files</code>
              <p class="cap-d">
                Drop config and seed data in at boot; build throwaway images that clean up after
                themselves.
              </p>
              <code class="cap-c">files: [{ path: "config.json", content: … }]</code>
            </div>
          </div>
        </div>
      </section>

      <section class="block">
        <div class="wrap">
          <div class="kicker"><b>//</b> how it works</div>
          <h2 class="h2">One binary, two planes.</h2>
          <p class="lead">
            A control plane that decides; a data plane that executes. Develop against the same API
            on your laptop — isolation switches on in production, not in your code.
          </p>
          <div class="flow rev">
            <div class="flow-node">
              <div class="fn-head"><span class="fn-name">your agent</span><span class="fn-meta">sdk · rest</span></div>
              <div class="fn-body">@workdir/sdk <i>·</i> pip install workdir <i>·</i> plain curl</div>
            </div>
            <div class="flow-link">
              <span class="fl-line"><i></i></span>
              <span class="fl-tag">https + api key</span>
            </div>
            <div class="flow-bin">
              <span class="bin-tag">one binary</span>
              <div class="flow-node">
                <div class="fn-head"><span class="fn-name">control plane</span><span class="fn-meta">decides</span></div>
                <div class="fn-body">scheduler <i>·</i> billing <i>·</i> image registry <i>·</i> preview proxy</div>
              </div>
              <div class="flow-link">
                <span class="fl-line"><i style="--fd:1.4s"></i></span>
                <span class="fl-tag"><b>Runtime</b> trait</span>
              </div>
              <div class="flow-node">
                <div class="fn-head"><span class="fn-name">data plane</span><span class="fn-meta">executes</span></div>
                <div class="fn-body">microVMs <i>·</i> jailer <i>·</i> vsock agent <i>·</i> hot pools</div>
              </div>
            </div>
            <div class="flow-caption"><span class="m-only">one binary · </span>start on one server · add nodes one command at a time</div>
          </div>
        </div>
      </section>

      <section class="block">
        <div class="wrap">
          <div class="kicker"><b>//</b> run it your way</div>
          <h2 class="h2">Managed cloud, or your own metal.</h2>
          <div class="split">
            <div class="splitcell rev">
              <div class="tag">cloud</div>
              <h3>workdir.dev</h3>
              <p class="grow">
                No infra, no images to build. Sign up, take a key, boot your first sandbox before
                the coffee order's ready.
              </p>
              <div><a class="btn primary" href="/signup">get an api key <span class="ar">→</span></a></div>
            </div>
            <div class="splitcell rev">
              <div class="tag">self-host</div>
              <h3>Your own server</h3>
              <p class="grow">
                One install script, one server, AGPL-3.0. The same binary that runs our cloud —
                scheduler, billing, previews and all.
              </p>
              <div class="install">
                <code id="code-install">curl -fsSL https://workdir.dev/install.sh | sudo bash</code>
                <button class="copy" data-copy="#code-install" type="button">copy</button>
              </div>
            </div>
          </div>
        </div>
      </section>

      <section class="cta">
        <div class="wrap">
          <div class="kicker" style="justify-content:center"><b>//</b> get started</div>
          <h2 class="h2">First sandbox in under a minute.</h2>
          <p>No credit card. No Kubernetes. No waitlist.</p>
          <div class="cta-row">
            <a class="btn primary" href="/signup">get an api key <span class="ar">→</span></a>
            <a class="btn" href="https://github.com/mv37-org/workdir">star on github</a>
          </div>
        </div>
      </section>
    `,
    { user },
  );
}

// ─────────────────────────────────────────────────────────────────────────────
// Auth
// ─────────────────────────────────────────────────────────────────────────────

export function authPage(mode: "login" | "signup", error?: string) {
  const isSignup = mode === "signup";
  return layout(
    isSignup ? "Sign up — workdir" : "Log in — workdir",
    html`
      <div class="auth-wrap">
        <form class="auth-card corners" method="post" action="${isSignup ? "/signup" : "/login"}">
          <h1>${isSignup ? "Create your account" : "Welcome back"}</h1>
          <p class="auth-sub">${isSignup ? "// 60 seconds to your first sandbox" : "// log in to manage your keys"}</p>
          ${error ? html`<div class="flash err"><b>err:</b>${error}</div>` : ""}
          <label for="email">email</label>
          <input id="email" name="email" type="email" placeholder="you@company.com" required autofocus />
          <label for="password">password</label>
          <input id="password" name="password" type="password" placeholder="${isSignup ? "8+ characters" : "••••••••"}" required minlength="8" />
          <div style="margin-top:26px">
            <button class="btn primary block" type="submit">${isSignup ? html`create account <span class="ar">→</span>` : html`log in <span class="ar">→</span>`}</button>
          </div>
        </form>
        <p class="auth-swap">
          ${isSignup
            ? html`already have an account? <a href="/login">log in</a>`
            : html`need an account? <a href="/signup">sign up</a>`}
        </p>
      </div>
    `,
  );
}

// ─────────────────────────────────────────────────────────────────────────────
// Dashboard
// ─────────────────────────────────────────────────────────────────────────────

export function dashboardPage(opts: {
  user: { email: string };
  orgId: string;
  keys: ApiKeyRow[];
  usage: Record<string, unknown> | null;
  newKey?: string;
  flash?: { kind: "ok" | "warn" | "err"; msg: string };
}) {
  const { user, orgId, keys, usage, newKey, flash } = opts;
  const balance = usage?.balance_usd as number | undefined;
  const active = usage?.active_sandboxes as number | undefined;
  const cost = usage?.total_cost_usd as number | undefined;
  const activeKeys = keys.filter((k) => !k.revoked).length;

  return layout(
    "Dashboard — workdir",
    html`
      <div class="wrap">
        <div class="dash-head">
          <div>
            <div class="crumbs">wd://<b>${orgId}</b>/keys</div>
            <h1>Dashboard</h1>
          </div>
          <div class="chips">
            <span class="chip">${user.email}</span>
            <span class="chip">org ${orgId}</span>
          </div>
        </div>

        ${flash
          ? html`<div class="flash ${flash.kind}"><b>${flash.kind}:</b>${flash.msg}</div>`
          : ""}

        ${newKey
          ? html`<div class="panel newkey corners">
              <div class="panel-head"><h2>new api key — copy it now</h2></div>
              <div class="panel-body">
                <div class="keyrow">
                  <div class="keycode" id="newkey">${newKey}</div>
                  <button class="copy" data-copy="#newkey" type="button">copy</button>
                </div>
                <p class="keynote">shown once — we store only the SHA-256 hash.</p>
              </div>
            </div>`
          : ""}

        <div class="tiles">
          <div class="tile">
            <span>credit balance</span>
            <b>${balance !== undefined ? `$${balance.toFixed(2)}` : "—"}</b>
            <i>${balance !== undefined ? "prepaid credits" : "daemon unreachable"}</i>
          </div>
          <div class="tile">
            <span>active sandboxes</span>
            <b>${active ?? "—"}</b>
            <i>billed per second while running</i>
          </div>
          <div class="tile">
            <span>spend this period</span>
            <b>${cost !== undefined ? `$${cost.toFixed(4)}` : "—"}</b>
            <i>metered by the daemon</i>
          </div>
        </div>

        <div class="panel">
          <div class="panel-head">
            <h2>api keys · ${activeKeys} active</h2>
            <form method="post" action="/dashboard/keys" class="inline-form">
              <input type="text" name="name" placeholder="key name, e.g. prod" />
              <button class="btn sm primary" type="submit">create key</button>
            </form>
          </div>
          <div class="panel-body">
            ${keys.length === 0
              ? html`<div class="empty">no keys yet — create one to start calling the API</div>`
              : html`<table>
                  <thead>
                    <tr><th>name</th><th>key</th><th>created</th><th>status</th><th></th></tr>
                  </thead>
                  <tbody>
                    ${keys.map(
                      (k) => html`<tr>
                        <td class="name">${k.name ?? "—"}</td>
                        <td><code>${k.prefix}…</code></td>
                        <td class="muted">${k.created_at.slice(0, 10)}</td>
                        <td>
                          ${k.revoked
                            ? html`<span class="status-off"><span class="dot off"></span>revoked</span>`
                            : html`<span class="status-on"><span class="dot on"></span>active</span>`}
                        </td>
                        <td style="text-align:right">
                          ${k.revoked
                            ? ""
                            : html`<form method="post" action="/dashboard/keys/${k.id}/revoke" style="margin:0">
                                <button class="btn sm danger" type="submit">revoke</button>
                              </form>`}
                        </td>
                      </tr>`,
                    )}
                  </tbody>
                </table>`}
          </div>
        </div>

        <div class="panel">
          <div class="panel-head">
            <h2>quickstart</h2>
            <button class="copy" data-copy="#code-quickstart" type="button">copy</button>
          </div>
          <pre class="code" id="code-quickstart" style="border:0">${raw(QUICKSTART)}</pre>
          <div class="panel-body" style="border-top:1px solid var(--line); padding:12px 20px">
            <span class="small muted" style="font-family:var(--mono); font-size:12px">
              full reference in the
              <a href="https://github.com/mv37-org/workdir/blob/main/docs/API.md">api docs</a>
            </span>
          </div>
        </div>
      </div>
    `,
    { user },
  );
}
