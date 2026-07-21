import { createContext } from "preact";
import { useContext } from "preact/hooks";
import type { RtcBackend } from "./rtc-backend";

export type TransportMode = "http" | "rtc";

export type TransportCtx = {
  mode: TransportMode;
  rtc: RtcBackend | null;
  phaseLabel?: string;
};

export const TransportContext = createContext<TransportCtx>({
  mode: "http",
  rtc: null,
});

export function useTransport(): TransportCtx {
  return useContext(TransportContext);
}
