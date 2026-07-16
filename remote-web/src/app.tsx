import { useEffect, useState } from "preact/hooks";
import { getToken, SessionInfo } from "./api";
import { CliPanel } from "./components/CliPanel";
import { ListPage } from "./pages/ListPage";

type Route =
  | { page: "list" }
  | { page: "session"; id: string; name: string; subtitle: string };

function parseRoute(): Route {
  const path = location.pathname.replace(/\/+$/, "") || "/";
  const m = path.match(/^\/s\/([^/]+)/);
  if (m) {
    const id = decodeURIComponent(m[1]);
    const q = new URLSearchParams(location.search);
    return {
      page: "session",
      id,
      name: q.get("name") || id.slice(0, 8),
      subtitle: q.get("sub") || "",
    };
  }
  return { page: "list" };
}

function writeEnabledFromMeta(): boolean {
  // 网关注入：<meta name="smelt-write" content="true|false">
  const el = document.querySelector('meta[name="smelt-write"]');
  return el?.getAttribute("content") === "true";
}

export function App() {
  const [route, setRoute] = useState<Route>(parseRoute);
  const [writeEnabled] = useState(writeEnabledFromMeta);

  useEffect(() => {
    getToken(); // 固化 query token
    const onPop = () => setRoute(parseRoute());
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  function goList() {
    const t = getToken();
    const url = t ? `/?token=${encodeURIComponent(t)}` : "/";
    history.pushState({}, "", url);
    setRoute({ page: "list" });
  }

  function openSession(s: SessionInfo) {
    const t = getToken();
    const sub =
      s.parent_session && s.parent_session !== s.name
        ? `${s.project} · ${s.parent_session}`
        : s.project;
    const q = new URLSearchParams();
    if (t) q.set("token", t);
    q.set("name", s.name);
    q.set("sub", sub);
    const url = `/s/${encodeURIComponent(s.id)}?${q.toString()}`;
    history.pushState({}, "", url);
    setRoute({ page: "session", id: s.id, name: s.name, subtitle: sub });
  }

  if (route.page === "session") {
    return (
      <CliPanel
        sessionId={route.id}
        name={route.name}
        subtitle={route.subtitle}
        writeEnabled={writeEnabled}
        onBack={goList}
      />
    );
  }

  return <ListPage onOpen={openSession} />;
}
