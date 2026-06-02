import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

// Build the whole dashboard into ONE self-contained index.html (JS + CSS
// inlined) so the signaling server can embed it via `include_str!`.
// `copy-dist.mjs` then copies it to ../src/admin_dashboard.html.
export default defineConfig({
  plugins: [react(), viteSingleFile()],
  build: {
    target: "es2021",
    outDir: "dist",
    emptyOutDir: true,
  },
});
