import { useEffect, useRef, useState } from "react";
import {
  Box,
  Button,
  Callout,
  Card,
  Flex,
  Grid,
  Heading,
  IconButton,
  Switch,
  Text,
  TextField,
} from "@radix-ui/themes";
import { CopyIcon, DesktopIcon, EnterIcon, InfoCircledIcon } from "@radix-ui/react-icons";
import {
  api,
  onClientError,
  onClientEvent,
  onClientStopped,
  type ClientEvent,
  type Role,
  type Settings,
} from "../api";

type Color = "gray" | "green" | "red" | "amber" | "blue";
interface LogLine {
  t: string;
  msg: string;
  color: Color;
}
interface SessionView {
  status: { text: string; color: Color };
  ready: string | null;
}

export default function Home({
  settings,
  deviceId,
  onOpenSettings,
}: {
  settings: Settings;
  deviceId: string;
  onOpenSettings: () => void;
}) {
  const [peer, setPeer] = useState(() => localStorage.getItem("spuria.peer") ?? "");
  const [listen, setListen] = useState(settings.default_listen);
  const [rdp, setRdp] = useState(settings.default_rdp);
  const [forceRelay, setForceRelay] = useState(settings.force_relay);
  const previousDefaults = useRef(settings);

  const [busy, setBusy] = useState(false);
  const [disconnecting, setDisconnecting] = useState(false);
  const [role, setRole] = useState<Role | null>(null);
  const [status, setStatus] = useState<{ text: string; color: Color }>({
    text: "idle",
    color: "gray",
  });
  const [ready, setReady] = useState<string | null>(null);
  const [log, setLog] = useState<LogLine[]>([]);
  const logEnd = useRef<HTMLDivElement>(null);

  const append = (msg: string, color: Color = "gray") =>
    setLog((l) => [...l.slice(-250), { t: new Date().toLocaleTimeString(), msg, color }]);

  useEffect(() => {
    if (busy) return;
    // Apply saved defaults without replacing per-connection edits or a running session's values.
    const previous = previousDefaults.current;
    setListen((value) => value === previous.default_listen ? settings.default_listen : value);
    setRdp((value) => value === previous.default_rdp ? settings.default_rdp : value);
    setForceRelay((value) => value === previous.force_relay ? settings.force_relay : value);
    previousDefaults.current = settings;
  }, [settings, busy]);

  useEffect(() => {
    logEnd.current?.scrollIntoView({ block: "end" });
  }, [log]);

  useEffect(() => {
    const sessions = new Map<string, SessionView>();
    const showSessions = (emptyColor: Color = "blue") => {
      const current = [...sessions.values()].reverse();
      setStatus(current[0]?.status ?? { text: "waiting for peer", color: emptyColor });
      setReady(current.find((session) => session.ready !== null)?.ready ?? null);
    };
    const updateSession = (id: string, update: Partial<SessionView>) => {
      const previous = sessions.get(id) ?? {
        status: { text: "negotiating…", color: "amber" as const },
        ready: null,
      };
      sessions.set(id, { ...previous, ...update });
      showSessions();
    };
    const handle = (e: ClientEvent) => {
      switch (e.kind) {
        case "registered":
          append("registered as " + e.device_id, "blue");
          setStatus({ text: "waiting for peer", color: "blue" });
          break;
        case "session_started":
          append(`session ${e.session_id} with peer ${e.peer_id}`);
          updateSession(e.session_id, { status: { text: "negotiating…", color: "amber" } });
          break;
        case "tunnel_up":
          append("tunnel up via " + e.path.toUpperCase(), "green");
          updateSession(e.session_id, { status: { text: "connected · " + e.path, color: "green" } });
          break;
        case "rdp_ready":
          updateSession(e.session_id, { ready: e.listen_addr });
          append("RDP ready at " + e.listen_addr, "green");
          break;
        case "host_bridging":
          append("bridging to local RDP " + e.rdp_addr, "green");
          updateSession(e.session_id, { status: { text: "hosting", color: "green" } });
          break;
        case "session_ended":
          append("session ended" + (e.error ? ": " + e.error : ""), e.error ? "amber" : "gray");
          sessions.delete(e.session_id);
          // The signaling client stays registered until explicitly disconnected.
          showSessions(e.error ? "amber" : "blue");
          break;
        case "error":
          append("error: " + e.message, "red");
          setStatus({ text: "error", color: "red" });
          break;
      }
    };
    const subs = [
      onClientEvent(handle),
      onClientStopped(() => {
        sessions.clear();
        setBusy(false);
        setRole(null);
        setReady(null);
        setStatus((current) => current.color === "red" ? current : { text: "idle", color: "gray" });
      }),
      onClientError((m) => {
        append("fatal: " + m, "red");
        setStatus({ text: "error", color: "red" });
        setReady(null);
      }),
    ];
    return () => subs.forEach((p) => p.then((u) => u()));
  }, []);

  const startSession = async (r: Role) => {
    if (busy || disconnecting) return;
    if (!settings.server.trim()) {
      append("no signaling server configured — open Settings", "red");
      onOpenSettings();
      return;
    }
    if (r === "controller" && !peer.trim()) {
      append("enter a remote device ID first", "amber");
      return;
    }
    localStorage.setItem("spuria.peer", peer);
    setBusy(true);
    setRole(r);
    setReady(null);
    setStatus({ text: "connecting…", color: "amber" });
    try {
      await api.connect({
        role: r,
        peer_id: r === "controller" ? peer.trim() : null,
        listen: listen.trim() || null,
        rdp: rdp.trim() || null,
        force_relay: forceRelay,
      });
    } catch (e) {
      append("connect failed: " + e, "red");
      setStatus({ text: "error", color: "red" });
      setBusy(false);
      setRole(null);
      if (String(e).includes("Settings")) onOpenSettings();
    }
  };

  const disconnect = async () => {
    setDisconnecting(true);
    try {
      await api.disconnect();
      setBusy(false);
      setRole(null);
      setReady(null);
      setStatus({ text: "idle", color: "gray" });
      append("disconnected");
    } catch (e) {
      append("disconnect failed: " + e, "red");
      setStatus({ text: "disconnect failed", color: "red" });
    } finally {
      setDisconnecting(false);
    }
  };

  const copyId = () => navigator.clipboard?.writeText(deviceId);

  return (
    <Flex direction="column" gap="4">
      <Grid columns={{ initial: "1", md: "2" }} gap="4">
        {/* ---- This device (被控 / Host) ---- */}
        <Card>
          <Flex direction="column" gap="3">
            <Flex align="center" gap="2">
              <DesktopIcon />
              <Heading size="4">This Device</Heading>
            </Flex>
            <Text size="2" color="gray">
              Share this ID so a teammate can control this machine.
            </Text>
            <Flex align="center" gap="2">
              <Text className="device-id">{deviceId}</Text>
              <IconButton variant="ghost" onClick={copyId} title="Copy ID">
                <CopyIcon />
              </IconButton>
            </Flex>
            <label>
              <Text size="2" color="gray">
                Local RDP service
              </Text>
              <TextField.Root
                value={rdp}
                placeholder="127.0.0.1:3389"
                onChange={(e) => setRdp(e.target.value)}
                disabled={busy}
              />
            </label>
            <Button
              onClick={() => startSession("host")}
              disabled={busy || disconnecting}
              variant="soft"
              color={role === "host" ? "green" : undefined}
            >
              <DesktopIcon /> {role === "host" ? "Hosting…" : "Allow remote control"}
            </Button>
          </Flex>
        </Card>

        {/* ---- Control remote (主控 / Controller) ---- */}
        <Card>
          <Flex direction="column" gap="3">
            <Flex align="center" gap="2">
              <EnterIcon />
              <Heading size="4">Control Remote Device</Heading>
            </Flex>
            <label>
              <Text size="2" color="gray">
                Remote device ID
              </Text>
              <TextField.Root
                value={peer}
                placeholder="e.g. 123456789"
                onChange={(e) => setPeer(e.target.value)}
                disabled={busy}
                size="3"
              />
            </label>
            <label>
              <Text size="2" color="gray">
                Local RDP listener
              </Text>
              <TextField.Root
                value={listen}
                placeholder="127.0.0.1:33389"
                onChange={(e) => setListen(e.target.value)}
                disabled={busy}
              />
            </label>
            <Text as="label" size="2">
              <Flex align="center" gap="2">
                <Switch checked={forceRelay} onCheckedChange={setForceRelay} disabled={busy} />
                Force relay (skip P2P)
              </Flex>
            </Text>
            <Button onClick={() => startSession("controller")} disabled={busy || disconnecting}>
              <EnterIcon /> {role === "controller" ? "Connecting…" : "Connect"}
            </Button>
          </Flex>
        </Card>
      </Grid>

      {ready && (
        <Callout.Root color="green">
          <Callout.Icon>
            <InfoCircledIcon />
          </Callout.Icon>
          <Callout.Text>
            Tunnel ready — point your RDP client (mstsc) at <b className="mono">{ready}</b>
          </Callout.Text>
        </Callout.Root>
      )}

      {/* ---- Activity ---- */}
      <Card>
        <Flex align="center" gap="3" mb="2">
          <Heading size="3">Activity</Heading>
          <Box style={{ flexGrow: 1 }} />
          <Text size="2" color={status.color}>
            ● {status.text}
          </Text>
          {busy && (
            <Button size="1" color="red" variant="soft" onClick={disconnect} disabled={disconnecting}>
              {disconnecting ? "Disconnecting…" : "Disconnect"}
            </Button>
          )}
          <Button size="1" variant="ghost" color="gray" onClick={() => setLog([])}>
            clear
          </Button>
        </Flex>
        <Box className="logbox">
          {log.length === 0 && (
            <Text size="2" color="gray">
              No activity yet.
            </Text>
          )}
          {log.map((l, i) => (
            <div key={i}>
              <span className="t">{l.t}</span>
              <Text size="2" color={l.color}>
                {l.msg}
              </Text>
            </div>
          ))}
          <div ref={logEnd} />
        </Box>
      </Card>
    </Flex>
  );
}
