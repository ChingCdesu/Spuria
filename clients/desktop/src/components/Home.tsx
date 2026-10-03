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
  onRdpLaunch,
  type ClientEvent,
  type RdpLaunchEvent,
  type Role,
  type Settings,
} from "../api";
import PortForwarding, { parseTcpPort, type ForwardView } from "./PortForwarding";

type Color = "gray" | "green" | "red" | "amber" | "blue";
interface LogLine {
  t: string;
  msg: string;
  color: Color;
}
interface SessionView {
  peerId?: string;
  connected: boolean;
  forwardingAvailable?: boolean;
  forwards: ForwardView[];
  status: { text: string; color: Color };
  ready: string | null;
  launch?: "launching" | RdpLaunchEvent["status"];
  launchMessage?: string;
}
interface ReadyView {
  address: string;
  launch: "manual" | "launching" | RdpLaunchEvent["status"];
  message?: string;
}

export default function Home({
  settings,
  deviceId,
  rdpLaunchSupported,
  onOpenSettings,
}: {
  settings: Settings;
  deviceId: string;
  rdpLaunchSupported: boolean;
  onOpenSettings: () => void;
}) {
  const [peer, setPeer] = useState(() => localStorage.getItem("spuria.peer") ?? "");
  const [listen, setListen] = useState(settings.default_listen);
  const [rdp, setRdp] = useState(settings.default_rdp);
  const [forceRelay, setForceRelay] = useState(settings.force_relay);
  const [openRdp, setOpenRdp] = useState(true);
  const [rdpUsername, setRdpUsername] = useState("");
  const [rdpPassword, setRdpPassword] = useState("");
  const [allowForwardPorts, setAllowForwardPorts] = useState("");
  const autoLaunchRequested = useRef(false);
  const previousDefaults = useRef(settings);
  const sessions = useRef(new Map<string, SessionView>());
  const listenersReady = useRef<Promise<void> | null>(null);
  const connectionAttempt = useRef(0);
  const [forwardSessions, setForwardSessions] = useState<(SessionView & { sessionId: string })[]>([]);

  const [busy, setBusy] = useState(false);
  const [disconnecting, setDisconnecting] = useState(false);
  const [role, setRole] = useState<Role | null>(null);
  const [status, setStatus] = useState<{ text: string; color: Color }>({
    text: "idle",
    color: "gray",
  });
  const [ready, setReady] = useState<ReadyView | null>(null);
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
    let active = true;
    const showSessions = (emptyColor: Color = "blue") => {
      const current = [...sessions.current.values()].reverse();
      setForwardSessions([...sessions.current].map(([sessionId, session]) => ({ sessionId, ...session })));
      setStatus(current[0]?.status ?? { text: "waiting for peer", color: emptyColor });
      const readySession = current.find((session) => session.ready !== null);
      setReady(readySession?.ready ? {
        address: readySession.ready,
        launch: readySession.launch ?? "manual",
        message: readySession.launchMessage,
      } : null);
    };
    const updateSession = (id: string, update: Partial<SessionView>) => {
      const previous = sessions.current.get(id) ?? {
        connected: false,
        forwards: [],
        status: { text: "negotiating…", color: "amber" as const },
        ready: null,
      };
      sessions.current.set(id, { ...previous, ...update });
      showSessions();
    };
    const handle = (e: ClientEvent) => {
      if (!active) return;
      switch (e.kind) {
        case "registered":
          append("registered as " + e.device_id, "blue");
          setStatus({ text: "waiting for peer", color: "blue" });
          break;
        case "session_started":
          append(`session ${e.session_id} with peer ${e.peer_id}`);
          updateSession(e.session_id, { peerId: e.peer_id, status: { text: "negotiating…", color: "amber" } });
          break;
        case "tunnel_up":
          append("tunnel up via " + e.path.toUpperCase(), "green");
          updateSession(e.session_id, { connected: true, status: { text: "connected · " + e.path, color: "green" } });
          break;
        case "rdp_ready":
          updateSession(e.session_id, {
            ready: e.listen_addr,
            launch: sessions.current.get(e.session_id)?.launch ?? (autoLaunchRequested.current ? "launching" : undefined),
          });
          append("RDP ready at " + e.listen_addr, "green");
          break;
        case "host_bridging":
          append("bridging to local RDP " + e.rdp_addr, "green");
          updateSession(e.session_id, { status: { text: "hosting", color: "green" } });
          break;
        case "port_forwarding_available":
          if (!sessions.current.has(e.session_id)) return;
          updateSession(e.session_id, { forwardingAvailable: e.available });
          break;
        case "port_forward_started": {
          const session = sessions.current.get(e.session_id);
          if (!session) return;
          updateSession(e.session_id, {
            forwards: [...session.forwards.filter((forward) => forward.forward_id !== e.forward_id), {
              forward_id: e.forward_id, listen_addr: e.listen_addr, remote_port: e.remote_port,
            }],
          });
          append(`TCP listening at ${e.listen_addr} → remote 127.0.0.1:${e.remote_port}`, "green");
          break;
        }
        case "port_forward_stopped": {
          const session = sessions.current.get(e.session_id);
          if (!session) return;
          updateSession(e.session_id, { forwards: session.forwards.filter((forward) => forward.forward_id !== e.forward_id) });
          append("TCP mapping stopped: " + e.forward_id);
          break;
        }
        case "port_forward_error": {
          const session = sessions.current.get(e.session_id);
          if (!session) return;
          updateSession(e.session_id, {
            forwards: session.forwards.map((forward) => forward.forward_id === e.forward_id ? { ...forward, error: e.message } : forward),
          });
          append(`TCP mapping ${e.forward_id}: ${e.message}`, "amber");
          break;
        }
        case "session_ended":
          append("session ended" + (e.error ? ": " + e.error : ""), e.error ? "amber" : "gray");
          sessions.current.delete(e.session_id);
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
      onRdpLaunch((event) => {
        // A late launch result must not resurrect a session that already ended.
        if (!active || !sessions.current.has(event.session_id)) return;
        updateSession(event.session_id, { launch: event.status, launchMessage: event.message });
        append(
          event.status === "launched" ? "Windows Remote Desktop started" : "Windows Remote Desktop could not be opened; connect manually using the ready address",
          event.status === "launched" ? "green" : "amber",
        );
      }),
      onClientStopped(() => {
        if (!active) return;
        sessions.current.clear();
        setForwardSessions([]);
        autoLaunchRequested.current = false;
        setRdpUsername("");
        setRdpPassword("");
        setBusy(false);
        setRole(null);
        setReady(null);
        setStatus((current) => current.color === "red" ? current : { text: "idle", color: "gray" });
      }),
      onClientError((m) => {
        if (!active) return;
        append("fatal: " + m, "red");
        setStatus({ text: "error", color: "red" });
        setReady(null);
      }),
    ];
    listenersReady.current = Promise.all(subs).then(() => undefined);
    // Keep registration failure handled even before the user presses Connect.
    void listenersReady.current.catch(() => {
      if (active) append("Could not subscribe to client events. Restart the application before connecting.", "red");
    });
    return () => {
      active = false;
      ++connectionAttempt.current;
      sessions.current.clear();
      subs.forEach((p) => void p.then((u) => u(), () => {}));
    };
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
    const hostPorts = r === "host" && allowForwardPorts.trim() ? allowForwardPorts.split(",").map(parseTcpPort) : [];
    if (hostPorts.some((port) => port === null) || new Set(hostPorts).size > 128) {
      append("allowed TCP ports must be a comma-separated list of up to 128 distinct ports between 1 and 65535", "amber");
      return;
    }
    const requestLaunch = r === "controller" && rdpLaunchSupported && openRdp;
    if (requestLaunch && (!rdpUsername.trim() || rdpPassword.length === 0)) {
      append("enter the remote Windows username and password to open Remote Desktop automatically", "amber");
      return;
    }
    localStorage.setItem("spuria.peer", peer);
    autoLaunchRequested.current = requestLaunch;
    const attempt = ++connectionAttempt.current;
    setBusy(true);
    setRole(r);
    setReady(null);
    setStatus({ text: "connecting…", color: "amber" });
    try {
      if (!listenersReady.current) throw new Error("Client event listeners are not ready. Try connecting again.");
      await listenersReady.current;
      if (attempt !== connectionAttempt.current) return;
      await api.connect({
        role: r,
        peer_id: r === "controller" ? peer.trim() : null,
        listen: listen.trim() || null,
        rdp: rdp.trim() || null,
        force_relay: forceRelay,
        rdp_launch: requestLaunch ? { username: rdpUsername.trim(), password: rdpPassword } : null,
        allow_forward_ports: [...new Set(hostPorts.filter((port): port is number => port !== null))],
      });
    } catch (e) {
      if (attempt !== connectionAttempt.current) return;
      autoLaunchRequested.current = false;
      append("connect failed: " + e, "red");
      setStatus({ text: "error", color: "red" });
      setBusy(false);
      setRole(null);
      if (String(e).includes("Settings")) onOpenSettings();
    } finally {
      if (attempt === connectionAttempt.current) setRdpPassword("");
    }
  };

  const disconnect = async () => {
    ++connectionAttempt.current;
    setRdpUsername("");
    setRdpPassword("");
    setDisconnecting(true);
    try {
      await api.disconnect();
      setBusy(false);
      setRole(null);
      setReady(null);
      sessions.current.clear();
      setForwardSessions([]);
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
            <label>
              <Text size="2" color="gray">Allowed TCP forwarding ports</Text>
              <TextField.Root
                value={allowForwardPorts}
                placeholder="e.g. 5432, 8080 (empty = deny all)"
                onChange={(event) => setAllowForwardPorts(event.target.value)}
                disabled={busy || disconnecting}
              />
            </label>
            <Text size="1" color="gray">
              Allow up to 128 ports on this device's 127.0.0.1 for this hosting run. Leave empty to disable TCP port forwarding.
            </Text>
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
                onChange={(e) => {
                  setPeer(e.target.value);
                  setRdpUsername("");
                  setRdpPassword("");
                }}
                disabled={busy || disconnecting}
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
            {rdpLaunchSupported && (
              <>
                <Text as="label" size="2">
                  <Flex align="center" gap="2">
                    <Switch
                      checked={openRdp}
                      onCheckedChange={(enabled) => {
                        setOpenRdp(enabled);
                        if (!enabled) setRdpPassword("");
                      }}
                      disabled={busy || disconnecting}
                    />
                    Open Windows Remote Desktop automatically
                  </Flex>
                </Text>
                <Text size="1" color="gray">Turn this off to use TCP port forwarding without opening or signing in to Windows Remote Desktop.</Text>
                {openRdp && (
                  <>
                    <label>
                      <Text size="2" color="gray">Remote Windows username</Text>
                      <TextField.Root
                        value={rdpUsername}
                        placeholder="PCNAME\user or DOMAIN\user"
                        autoComplete="off"
                        spellCheck={false}
                        onChange={(e) => setRdpUsername(e.target.value)}
                        disabled={busy || disconnecting}
                      />
                    </label>
                    <label>
                      <Text size="2" color="gray">Remote Windows password</Text>
                      <TextField.Root
                        type="password"
                        value={rdpPassword}
                        autoComplete="off"
                        onChange={(e) => setRdpPassword(e.target.value)}
                        disabled={busy || disconnecting}
                      />
                    </label>
                    <Text size="1" color="gray">
                      Use the remote PC's actual computer or domain name. These credentials are used for this connection only.
                      Windows may still show sign-in or security prompts.
                    </Text>
                  </>
                )}
              </>
            )}
            <Button onClick={() => startSession("controller")} disabled={busy || disconnecting}>
              <EnterIcon /> {role === "controller" ? "Connecting…" : "Connect"}
            </Button>
          </Flex>
        </Card>
      </Grid>

      {ready && (
        <Callout.Root color={ready.launch === "failed" ? "amber" : ready.launch === "launching" ? "blue" : "green"}>
          <Callout.Icon>
            <InfoCircledIcon />
          </Callout.Icon>
          <Callout.Text>
            {ready.launch === "launching" ? (
              <>Tunnel ready. Opening Windows Remote Desktop for <b className="mono">{ready.address}</b>…</>
            ) : ready.launch === "launched" ? (
              <>Windows Remote Desktop started for <b className="mono">{ready.address}</b>. Complete any Windows prompts in that window.</>
            ) : ready.launch === "failed" ? (
              <>Tunnel ready at <b className="mono">{ready.address}</b>, but Windows Remote Desktop could not be opened.
                {ready.message && <> {ready.message}</>} Connect an RDP client manually to this address.</>
            ) : (
              <>Tunnel ready. Connect your RDP client to <b className="mono">{ready.address}</b>.</>
            )}
          </Callout.Text>
        </Callout.Root>
      )}

      {busy && role === "controller" && forwardSessions.map((session) => (
        <PortForwarding
          key={session.sessionId}
          sessionId={session.sessionId}
          peerId={session.peerId}
          connected={session.connected}
          available={session.forwardingAvailable}
          forwards={session.forwards}
          disabled={disconnecting}
          isSessionActive={(id) => sessions.current.has(id)}
        />
      ))}

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
