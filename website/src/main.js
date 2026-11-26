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
