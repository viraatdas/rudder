import { render } from "preact";
import { BoardView, ProjectIndex } from "./components";

declare global {
  interface Window {
    __RUDDER_SLUG__?: string;
    __RUDDER_TOKEN__?: string;
    __RUDDER_CONTROL_MODE__?: "projector" | "scheduler";
    __RUDDER_CAN_MUTATE__?: boolean;
  }
}

function App() {
  const slug = (window.__RUDDER_SLUG__ ?? "").trim();
  return slug ? <BoardView slug={slug} /> : <ProjectIndex />;
}

const mount = document.getElementById("app");
if (mount) {
  render(<App />, mount);
}
