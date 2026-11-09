// How pages arrive. Everything here is decoration over content that is already
// complete: asked for less motion, or with this file absent, the page simply is.
import { wireReadout } from "./figures.js";

export const calm = matchMedia("(prefers-reduced-motion: reduce)").matches;

const EASE = (t) => 1 - (1 - t) ** 4;

// Figures run up to their value once, the first time a page is shown.
function countUp(node) {
  const text = node.textContent;
  const match = /[\d,]*\.?\d+/.exec(text);
  if (!match) return;
  const target = Number(match[0].replace(/,/g, ""));
  if (!Number.isFinite(target) || target === 0) return;
  const decimals = (match[0].split(".")[1] || "").length;
  const format = (v) => v.toLocaleString("en-US", { minimumFractionDigits: decimals, maximumFractionDigits: decimals });
  const start = performance.now();
  const step = (now) => {
    const t = Math.min(1, (now - start) / 900);
    node.textContent = text.slice(0, match.index) + format(target * EASE(t)) + text.slice(match.index + match[0].length);
    if (t < 1 && node.isConnected) requestAnimationFrame(step);
    else if (t >= 1) node.textContent = text;
  };
  requestAnimationFrame(step);
}

const revealer = "IntersectionObserver" in window && !calm
  ? new IntersectionObserver((entries) => {
    for (const e of entries) {
      if (e.isIntersecting) {
        e.target.classList.add("in");
        revealer.unobserve(e.target);
      }
    }
  }, { rootMargin: "0px 0px -6% 0px" })
  : null;

let dial = null;
async function dialSvg() {
  dial ||= fetch("brand/dial.svg").then((r) => (r.ok ? r.text() : ""));
  return dial;
}

// Dress a freshly drawn page. `entering` is false for a quiet refresh in place.
export function dress(root, entering) {
  root.querySelectorAll(".cells24").forEach((row) => [...row.children].forEach((c, h) => c.style.setProperty("--h", h)));
  root.querySelectorAll(".rose path").forEach((p, i) => p.style.setProperty("--i", i));
  root.querySelectorAll(".ip-atmos-dial").forEach(async (d) => { d.innerHTML = await dialSvg(); });
  root.querySelectorAll(".heat-read").forEach((line) => wireReadout(line.previousElementSibling, line, line.textContent));
  rail(root);
  if (!entering) {
    root.classList.remove("enter");
    return;
  }
  root.classList.remove("enter");
  void root.offsetWidth;
  root.classList.add("enter");
  if (calm) return;
  root.querySelectorAll(".ip-readout dd").forEach(countUp);
  if (revealer) {
    root.querySelectorAll("section").forEach((s) => { s.classList.add("rv"); revealer.observe(s); });
  }
}

// Long pages get a strip of their sections that stays under the masthead and
// marks the one being read. The page's address is its route, so these scroll
// rather than link.
let railWatch = null;
function rail(root) {
  railWatch?.disconnect();
  railWatch = null;
  const heads = [...root.querySelectorAll("section > h2[id][data-rail]")];
  const head = root.querySelector(":scope > header");
  if (heads.length < 3 || !head) return;
  const nav = document.createElement("nav");
  nav.className = "rail";
  nav.setAttribute("aria-label", "On this page");
  nav.innerHTML = `<span class="lbl">On this page</span>${heads.map((h) => `<button type="button" data-to="${h.id}">${h.dataset.rail}</button>`).join("")}`;
  head.after(nav);
  nav.addEventListener("click", (ev) => {
    const to = ev.target.closest("button")?.dataset.to;
    if (!to) return;
    const section = document.getElementById(to).closest("section");
    section.classList.add("in");
    section.scrollIntoView({ block: "start", behavior: calm ? "auto" : "smooth" });
  });
  if (!("IntersectionObserver" in window)) return;
  const buttons = Object.fromEntries([...nav.querySelectorAll("button")].map((b) => [b.dataset.to, b]));
  const seen = new Set();
  railWatch = new IntersectionObserver((entries) => {
    for (const e of entries) {
      const id = e.target.querySelector("h2").id;
      if (e.isIntersecting) seen.add(id);
      else seen.delete(id);
    }
    const current = heads.find((h) => seen.has(h.id));
    for (const [id, b] of Object.entries(buttons)) {
      if (id === current?.id) b.setAttribute("aria-current", "true");
      else b.removeAttribute("aria-current");
    }
  }, { rootMargin: "-18% 0px -55% 0px" });
  heads.forEach((h) => railWatch.observe(h.closest("section")));
}

// The ember tick under the current section, sliding between them.
export function moveTick() {
  const nav = document.querySelector(".ip-masthead-bar nav");
  if (!nav) return;
  let tick = nav.querySelector(".nav-tick");
  if (!tick) {
    tick = document.createElement("i");
    tick.className = "nav-tick";
    tick.setAttribute("aria-hidden", "true");
    nav.append(tick);
  }
  const current = nav.querySelector('[aria-current="page"]');
  if (!current) { tick.style.opacity = 0; return; }
  tick.style.opacity = 1;
  tick.style.transform = `translateX(${current.offsetLeft}px) scaleX(${current.offsetWidth / 100})`;
}

// A faint reading of the page's own numbers, drawn as the mark is drawn: bars
// mirrored about one axis. Texture only: no labels, and only real series.
export function reading(series) {
  if (!series || !series.length) return "";
  const n = Math.min(series.length, 96);
  const data = series.slice(-n);
  const max = Math.max(1, ...data);
  const step = 1000 / n;
  // Thin bars however few the samples: a few wide ones would read as blocks.
  const bars = data.map((v, i) => {
    if (!v) return "";
    const h = Math.max(3, 150 * Math.sqrt(v / max));
    return `<rect x="${(i * step + step / 2 - 2).toFixed(1)}" y="${(100 - h / 2).toFixed(1)}" width="4" height="${h.toFixed(1)}" rx="2"/>`;
  }).join("");
  return `<div class="ip-backdrop"><svg viewBox="0 0 1000 200" preserveAspectRatio="xMidYMid slice" fill="currentColor">${bars}</svg></div>`;
}

export function atmosphere(series) {
  return `<div class="ip-atmos-bg" aria-hidden="true">${reading(series)}<i class="ip-fog f1"></i><i class="ip-fog f2"></i><i class="ip-fog f3"></i><div class="ip-atmos-dial"></div></div>`;
}
