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

// Offer the download for the visitor's system. Without JavaScript, and on
// systems cagents has no build for, the buttons stay on the install guide.
const offers = JSON.parse(document.getElementById("download-offers")?.textContent ?? "{}");
currentPlatform().then((platform) => {
  document.documentElement.dataset.platform = platform;
  const offer = offers[platform];
  for (const button of document.querySelectorAll("[data-download-button]")) {
    if (offer) {
      button.href = offer.href;
      button.textContent = offer.button;
    }
    // cagents has no iPhone app, so the hero explains that instead.
    if (platform === "ios") button.hidden = true;
  }
  for (const note of document.querySelectorAll("[data-note]")) note.hidden = note.dataset.note !== platform;
  document.querySelector(`[data-download-item="${platform}"]`)?.classList.add("is-yours");

  // Show the reader's own keyboard shortcuts; the page shows Linux's until
  // then. Intel Macs and iPads, which report "MacIntel", get the Mac keys too.
  const mac = platform === "mac" || /^mac/i.test(navigator.platform ?? "");
  for (const keys of document.querySelectorAll(".keys[data-platform]")) {
    keys.hidden = keys.dataset.platform !== (mac ? "mac" : "linux");
  }
});
