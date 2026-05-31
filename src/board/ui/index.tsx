import { render } from "preact";
import { BoardView, ProjectIndex } from "./components";

declare global {
  interface Window {
    __RUDDER_SLUG__?: string;
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
