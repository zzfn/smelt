import { useEffect, useState } from "preact/hooks";
import { getToken, SessionInfo } from "./api";
import { CliPanel } from "./components/CliPanel";
import { ListPage } from "./pages/ListPage";
import { parseRtcQuery } from "./transport/rtc-peer";
import { startRtcBackend, type RtcBackend } from "./transport/rtc-backend";
import { TransportContext } from "./transport/TransportContext";
import type { RtcConnPhase } from "./transport/types";

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
  const el = document.querySelector('meta[name="smelt-write"]');
  return el?.getAttribute("content") === "true";
}

export function App() {
  const [route, setRoute] = useState<Route>(parseRoute);
  const [writeEnabled] = useState(writeEnabledFromMeta);
  const wantRtc = !!parseRtcQuery();
  const [rtc, setRtc] = useState<RtcBackend | null>(null);
  const [rtcPhase, setRtcPhase] = useState<string>(wantRtc ? "signaling…" : "");
  const [rtcErr, setRtcErr] = useState<string | null>(null);

  useEffect(() => {
    getToken();
    const onPop = () => setRoute(parseRoute());
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  useEffect(() => {
    if (!wantRtc) return;
    let cancelled = false;
    let backend: RtcBackend | null = null;
    void (async () => {
      try {
        backend = await startRtcBackend((p: RtcConnPhase, detail?: string) => {
          if (cancelled) return;
          setRtcPhase(detail ? `${p}: ${detail}` : p);
          if (p === "failed") setRtcErr(detail || "连接失败");
        });
        if (cancelled) {
          backend?.close();
          return;
        }
        if (!backend) {
          setRtcErr("跨网参数不完整（需要 room、k、signal）");
          return;
        }
        setRtc(backend);
        setRtcPhase("connected");
      } catch (e) {
        if (!cancelled) {
          setRtcErr(e instanceof Error ? e.message : String(e));
        }
      }
    })();
    return () => {
      cancelled = true;
      backend?.close();
    };
  }, [wantRtc]);

  function goList() {
    const t = getToken();
    const q = new URLSearchParams(location.search);
    // 保留跨网 query
    if (t) q.set("token", t);
    const qs = q.toString();
    const url = qs ? `/?${qs}` : "/";
    history.pushState({}, "", url);
    setRoute({ page: "list" });
  }

  function openSession(s: SessionInfo) {
    const t = getToken();
    const sub =
      s.parent_session && s.parent_session !== s.name
        ? `${s.project} · ${s.parent_session}`
        : s.project;
    const q = new URLSearchParams(location.search);
    if (t) q.set("token", t);
    q.set("name", s.name);
    q.set("sub", sub);
    const url = `/s/${encodeURIComponent(s.id)}?${q.toString()}`;
    history.pushState({}, "", url);
    setRoute({ page: "session", id: s.id, name: s.name, subtitle: sub });
  }

  if (wantRtc && !rtc && !rtcErr) {
    return (
      <div class="mx-auto max-w-lg px-4 py-16 text-center text-sm text-muted">
        <p class="mb-2 font-medium text-fg">正在建立跨网连接…</p>
        <p class="text-xs">{rtcPhase}</p>
      </div>
    );
  }

  if (wantRtc && rtcErr && !rtc) {
    return (
      <div class="mx-auto max-w-lg px-4 py-16 text-center text-sm">
        <p class="mb-2 font-semibold text-danger">跨网连接失败</p>
        <p class="text-muted">{rtcErr}</p>
      </div>
    );
  }

  const transportValue = {
    mode: (wantRtc && rtc ? "rtc" : "http") as "rtc" | "http",
    rtc,
    phaseLabel: wantRtc ? rtcPhase : undefined,
  };

  return (
    <TransportContext.Provider value={transportValue}>
      {route.page === "session" ? (
        <CliPanel
          sessionId={route.id}
          name={route.name}
          subtitle={route.subtitle}
          writeEnabled={
            wantRtc && rtc ? rtc.writeEnabled() : writeEnabled || wantRtc
          }
          onBack={goList}
        />
      ) : (
        <ListPage onOpen={openSession} />
      )}
    </TransportContext.Provider>
  );
}
