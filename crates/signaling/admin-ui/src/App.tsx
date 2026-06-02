import { useCallback, useEffect, useState } from "react";
import {
  Badge,
  Box,
  Button,
  Card,
  Container,
  Flex,
  Heading,
  Table,
  Tabs,
  Text,
  TextField,
} from "@radix-ui/themes";
import {
  apiGet,
  fmtDur,
  kick,
  Unauthorized,
  type Audit,
  type Device,
  type Session,
  type Stats,
} from "./api";

const TOKEN_KEY = "spuria.admin.token";

export default function App() {
  const [token, setToken] = useState(() => localStorage.getItem(TOKEN_KEY) ?? "");
  const [authed, setAuthed] = useState(false);

  if (!authed) {
    return <Login token={token} setToken={setToken} onAuthed={() => setAuthed(true)} />;
  }
  return <Dashboard token={token} onLogout={() => { localStorage.removeItem(TOKEN_KEY); setAuthed(false); }} />;
}

function Login({
  token,
  setToken,
  onAuthed,
}: {
  token: string;
  setToken: (t: string) => void;
  onAuthed: () => void;
}) {
  const [err, setErr] = useState("");
  const submit = async () => {
    setErr("");
    try {
      await apiGet<Stats>("/api/stats", token);
      localStorage.setItem(TOKEN_KEY, token);
      onAuthed();
    } catch (e) {
      setErr(e instanceof Unauthorized ? "Invalid admin token" : String(e));
    }
  };
  return (
    <Container size="1" px="4" py="9">
      <Card>
        <Flex direction="column" gap="3">
          <Heading size="5">Spuria Admin</Heading>
          <Text size="2" color="gray">
            Enter the admin bearer token (the signaling server's <code>--admin-token</code>).
          </Text>
          <TextField.Root
            type="password"
            placeholder="admin token"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
          />
          {err && (
            <Text size="2" color="red">
              {err}
            </Text>
          )}
          <Button onClick={submit}>Sign in</Button>
        </Flex>
      </Card>
    </Container>
  );
}

function Dashboard({ token, onLogout }: { token: string; onLogout: () => void }) {
  const [stats, setStats] = useState<Stats>({ online: 0, sessions: 0 });
  const [devices, setDevices] = useState<Device[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [audit, setAudit] = useState<Audit[]>([]);
  const [err, setErr] = useState("");

  const refresh = useCallback(async () => {
    try {
      const [st, dv, se, au] = await Promise.all([
        apiGet<Stats>("/api/stats", token),
        apiGet<Device[]>("/api/devices", token),
        apiGet<Session[]>("/api/sessions", token),
        apiGet<Audit[]>("/api/audit?limit=200", token),
      ]);
      setStats(st);
      setDevices(dv);
      setSessions(se);
      setAudit(au);
      setErr("");
    } catch (e) {
      if (e instanceof Unauthorized) onLogout();
      else setErr(String(e));
    }
  }, [token, onLogout]);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 2000);
    return () => clearInterval(id);
  }, [refresh]);

  const doKick = async (id: string) => {
    try {
      await kick(id, token);
      refresh();
    } catch (e) {
      if (e instanceof Unauthorized) onLogout();
    }
  };

  return (
    <Container size="3" px="4" py="5">
      <Flex align="center" gap="3" mb="4">
        <Heading size="6">Spuria Admin</Heading>
        <Badge color="green" variant="soft">
          {stats.online} online
        </Badge>
        <Badge color="indigo" variant="soft">
          {stats.sessions} sessions
        </Badge>
        <Box style={{ flexGrow: 1 }} />
        {err && (
          <Text size="2" color="red">
            {err}
          </Text>
        )}
        <Button variant="soft" color="gray" onClick={onLogout}>
          Sign out
        </Button>
      </Flex>

      <Tabs.Root defaultValue="devices">
        <Tabs.List>
          <Tabs.Trigger value="devices">Devices ({devices.length})</Tabs.Trigger>
          <Tabs.Trigger value="sessions">Sessions ({sessions.length})</Tabs.Trigger>
          <Tabs.Trigger value="audit">Audit ({audit.length})</Tabs.Trigger>
        </Tabs.List>

        <Box pt="3">
          <Tabs.Content value="devices">
            <Table.Root variant="surface">
              <Table.Header>
                <Table.Row>
                  <Table.ColumnHeaderCell>Device ID</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Public key</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Uptime</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Idle</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell />
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {devices.map((d) => (
                  <Table.Row key={d.device_id}>
                    <Table.RowHeaderCell className="mono">{d.device_id}</Table.RowHeaderCell>
                    <Table.Cell className="mono">{d.noise_pubkey}</Table.Cell>
                    <Table.Cell>{fmtDur(d.uptime_secs)}</Table.Cell>
                    <Table.Cell>{fmtDur(d.idle_secs)}</Table.Cell>
                    <Table.Cell>
                      <Button size="1" color="red" variant="soft" onClick={() => doKick(d.device_id)}>
                        Kick
                      </Button>
                    </Table.Cell>
                  </Table.Row>
                ))}
                {devices.length === 0 && <EmptyRow cols={5} />}
              </Table.Body>
            </Table.Root>
          </Tabs.Content>

          <Tabs.Content value="sessions">
            <Table.Root variant="surface">
              <Table.Header>
                <Table.Row>
                  <Table.ColumnHeaderCell>Session</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Controller</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Host</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Path</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Age</Table.ColumnHeaderCell>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {sessions.map((s) => (
                  <Table.Row key={s.session_id}>
                    <Table.RowHeaderCell className="mono">{s.session_id}</Table.RowHeaderCell>
                    <Table.Cell className="mono">{s.controller}</Table.Cell>
                    <Table.Cell className="mono">{s.host}</Table.Cell>
                    <Table.Cell>
                      <Badge color={s.relayed ? "amber" : "green"} variant="soft">
                        {s.relayed ? "relay" : "p2p"}
                      </Badge>
                    </Table.Cell>
                    <Table.Cell>{fmtDur(s.age_secs)}</Table.Cell>
                  </Table.Row>
                ))}
                {sessions.length === 0 && <EmptyRow cols={5} />}
              </Table.Body>
            </Table.Root>
          </Tabs.Content>

          <Tabs.Content value="audit">
            <Table.Root variant="surface">
              <Table.Header>
                <Table.Row>
                  <Table.ColumnHeaderCell>Time</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Event</Table.ColumnHeaderCell>
                  <Table.ColumnHeaderCell>Detail</Table.ColumnHeaderCell>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {[...audit].reverse().map((a, i) => (
                  <Table.Row key={i}>
                    <Table.Cell>{new Date(a.ts_ms).toLocaleTimeString()}</Table.Cell>
                    <Table.Cell>
                      <Badge variant="soft">{a.kind}</Badge>
                    </Table.Cell>
                    <Table.Cell className="mono">{a.detail}</Table.Cell>
                  </Table.Row>
                ))}
                {audit.length === 0 && <EmptyRow cols={3} />}
              </Table.Body>
            </Table.Root>
          </Tabs.Content>
        </Box>
      </Tabs.Root>
    </Container>
  );
}

function EmptyRow({ cols }: { cols: number }) {
  return (
    <Table.Row>
      <Table.Cell colSpan={cols}>
        <Text size="2" color="gray">
          (none)
        </Text>
      </Table.Cell>
    </Table.Row>
  );
}
