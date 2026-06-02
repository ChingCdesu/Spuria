// Copy the single-file build output into the signaling crate so `include_str!`
// embeds it. Run automatically by `npm run build`.
import { copyFileSync } from "node:fs";

const src = new URL("./dist/index.html", import.meta.url);
const dst = new URL("../src/admin_dashboard.html", import.meta.url);
copyFileSync(src, dst);
console.log("admin-ui: copied dist/index.html -> crates/signaling/src/admin_dashboard.html");
