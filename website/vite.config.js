import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  base: "/cagents/",
  plugins: [react()],
});

# Automated fix for issue #2069: The next mouse click is dropped after pressing Enter on a focused button
