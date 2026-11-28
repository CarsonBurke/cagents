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

// Show the reader's own keyboard shortcuts. Without JavaScript, or on any
// other platform, the page shows Linux's. iPadOS also reports "MacIntel"
// and gets the Mac keys, which suit an iPad with a keyboard.
const platform = navigator.userAgentData?.platform || navigator.platform || "";
if (/^mac/i.test(platform)) {
  for (const keys of document.querySelectorAll(".keys[data-platform]")) {
    keys.hidden = keys.dataset.platform !== "mac";
  }
}
