// App entry point: the multi-terminal shell.
import { App } from "./app";

const root = document.getElementById("app")!;
const app = new App({ root });

window.addEventListener("beforeunload", () => app.closeAll());

void app.mount();
