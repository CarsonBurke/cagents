// The only script on the page: the install command's Copy button.
for (const button of document.querySelectorAll("[data-copy-target]")) {
  const target = document.getElementById(button.dataset.copyTarget);
  const status = button.closest(".command")?.querySelector(".copy-status");
  let reset;
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(target.textContent);
      button.textContent = "Copied";
      if (status) status.textContent = "Install command copied to the clipboard.";
    } catch {
      // Without clipboard access, select the command so it can be copied by hand.
      getSelection().selectAllChildren(target);
      if (status) status.textContent = "Press Ctrl+C to copy the selected command.";
    }
    clearTimeout(reset);
    reset = setTimeout(() => {
      button.textContent = "Copy";
      if (status) status.textContent = "";
    }, 2000);
  });
}
