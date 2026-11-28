import { currentPlatform } from "./platform.js";

const viewer = document.querySelector(".screenshot-viewer");
const imageRegion = viewer.querySelector(".viewer-image");
const expandedImage = imageRegion.querySelector("img");
const fullSizeLink = viewer.querySelector(".viewer-full-size");
let trigger;

if (typeof viewer.showModal === "function") {
  for (const link of document.querySelectorAll(".shot-expand")) {
    link.setAttribute("aria-haspopup", "dialog");
    link.addEventListener("click", (event) => {
      if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      event.preventDefault();

      const thumbnail = link.querySelector("img");
      const source = thumbnail.currentSrc || link.href;
      expandedImage.src = source;
      expandedImage.alt = thumbnail.alt;
      fullSizeLink.href = source;
      trigger = link;
      viewer.showModal();
      imageRegion.scrollTop = 0;
    });
  }

  viewer.addEventListener("click", (event) => {
    if (event.target === viewer) viewer.close();
  });

  viewer.addEventListener("close", () => trigger.focus({ preventScroll: true }));
}

// The sticky navigation gains its bottom border once the page scrolls under it.
const nav = document.querySelector(".nav");
const sentinel = document.querySelector(".scroll-sentinel");
if (nav && sentinel && "IntersectionObserver" in window) {
  new IntersectionObserver(([entry]) => {
    nav.classList.toggle("is-scrolled", !entry.isIntersecting);
  }).observe(sentinel);
}

// The head script names the visitor's platform before first paint. Where the
// browser reports the processor architecture, refine it: an Intel Mac or an
// Arm Linux machine has no cagents build.
const root = document.documentElement;
const showKeys = () => {
  // Intel Macs and iPads report "MacIntel" and use the Mac's keys too.
  const mac = root.dataset.platform === "mac" || /^mac/i.test(navigator.platform ?? "");
  for (const keys of document.querySelectorAll(".keys[data-platform]")) {
    keys.hidden = keys.dataset.platform !== (mac ? "mac" : "linux");
  }
};
showKeys();
currentPlatform().then((platform) => {
  if (platform === root.dataset.platform) return;
  root.dataset.platform = platform;
  showKeys();
});
