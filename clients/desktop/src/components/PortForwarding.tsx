import { useEffect, useRef, useState } from "react";
import { Button, Callout, Card, Flex, Grid, Heading, Text, TextField } from "@radix-ui/themes";
import { api, type ForwardInfo } from "../api";

export interface ForwardView extends ForwardInfo {
  error?: string;
}

export function parseTcpPort(value: string): number | null {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) return null;
  const port = Number(trimmed);
  return Number.isInteger(port) && port >= 1 && port <= 65535 ? port : null;
}

export default function PortForwarding({
  sessionId, peerId, connected, available, forwards, disabled, isSessionActive,
}: {
  sessionId: string;
  peerId?: string;
  connected: boolean;
  available?: boolean;
  forwards: ForwardView[];
  disabled: boolean;
  isSessionActive: (sessionId: string) => boolean;
}) {
  const [localPort, setLocalPort] = useState("");
  const [remotePort, setRemotePort] = useState("");
  const [adding, setAdding] = useState(false);
  const [stopping, setStopping] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  const stillActive = () => mounted.current && isSessionActive(sessionId);
  const canAdd = connected && available === true && !disabled && !adding && forwards.length < 32;
  const add = async () => {
    if (!canAdd || !stillActive()) return;
    const local = parseTcpPort(localPort);
    const remote = parseTcpPort(remotePort);
    if (local === null || remote === null) {
      setError("Enter local and remote TCP ports between 1 and 65535.");
      return;
    }
    setAdding(true);
    setError(null);
    try {
      await api.startPortForward(sessionId, `127.0.0.1:${local}`, remote);
      // Only live events add rows. A late command result cannot restore a stopped mapping.
      if (stillActive()) {
        setLocalPort("");
        setRemotePort("");
      }
    } catch (e) {
      if (stillActive()) setError("Could not add mapping: " + String(e));
    } finally {
      if (stillActive()) setAdding(false);
    }
  };
  const stop = async (forwardId: string) => {
    if (disabled || stopping.includes(forwardId) || !stillActive()) return;
    setStopping((current) => [...current, forwardId]);
    setError(null);
    try {
      await api.stopPortForward(sessionId, forwardId);
    } catch (e) {
      if (stillActive()) setError("Could not stop mapping: " + String(e));
    } finally {
      if (stillActive()) setStopping((current) => current.filter((id) => id !== forwardId));
    }
  };

  return (
    <Card>
      <Flex direction="column" gap="3">
        <Heading size="4">TCP Port Forwarding</Heading>
        <Text size="2" color="gray">Remote device: {peerId ?? sessionId}</Text>
        {available === false ? (
          <Callout.Root color="amber">
            <Callout.Text>Port forwarding is unavailable for this connection. Update both clients and the signaling server, then reconnect. RDP remains available.</Callout.Text>
          </Callout.Root>
        ) : !connected || available !== true ? (
          <Text size="2" color="gray">Waiting for the tunnel and port forwarding support…</Text>
        ) : (
          <>
            <Text size="2" color="gray">
              Forward this computer's 127.0.0.1 port to an allowed TCP port on the remote device's 127.0.0.1.
              A listening mapping does not confirm that the remote service is available.
            </Text>
            <Grid columns={{ initial: "1", sm: "2" }} gap="3">
              <label>
                <Text size="2">Local TCP port</Text>
                <TextField.Root value={localPort} onChange={(event) => setLocalPort(event.target.value)} placeholder="e.g. 15432" inputMode="numeric" disabled={disabled || adding} />
              </label>
              <label>
                <Text size="2">Remote TCP port</Text>
                <TextField.Root value={remotePort} onChange={(event) => setRemotePort(event.target.value)} placeholder="e.g. 5432" inputMode="numeric" disabled={disabled || adding} />
              </label>
            </Grid>
            <Button onClick={add} disabled={!canAdd}>{adding ? "Adding…" : "Add mapping"}</Button>
            {forwards.length >= 32 && <Text size="2" color="amber">Stop a mapping before adding another (32 maximum).</Text>}
          </>
        )}
        {error && <Text size="2" color="red" role="alert">{error}</Text>}
        {forwards.length === 0 && available === true && <Text size="2" color="gray">No active mappings.</Text>}
        {forwards.map((forward) => (
          <Flex key={forward.forward_id} direction="column" gap="1" style={{ borderTop: "1px solid var(--gray-5)", paddingTop: 12 }}>
            <Flex align="center" justify="between" gap="3" wrap="wrap">
              <Text size="2" className="mono">{forward.listen_addr} → remote 127.0.0.1:{forward.remote_port}</Text>
              <Button size="1" color="red" variant="soft" disabled={disabled || stopping.includes(forward.forward_id)} onClick={() => stop(forward.forward_id)}>
                {stopping.includes(forward.forward_id) ? "Stopping…" : "Stop"}
              </Button>
            </Flex>
            <Text size="1" color={forward.error ? "amber" : "green"}>Listening{forward.error ? " · last error below" : ""}</Text>
            {forward.error && <Text size="2" color="amber" role="alert">{forward.error}</Text>}
          </Flex>
        ))}
        {forwards.length > 0 && <Text size="1" color="gray">Stop closes the mapping's active connections. Disconnect releases all mappings.</Text>}
      </Flex>
    </Card>
  );
}
