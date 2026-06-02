import { useState, type ReactNode } from "react";
import {
  Badge,
  Button,
  Card,
  Flex,
  Heading,
  Select,
  Switch,
  Text,
  TextField,
} from "@radix-ui/themes";
import { api, type AppInfo, type Settings } from "../api";

export default function SettingsView({
  settings,
  info,
  onSaved,
}: {
  settings: Settings;
  info: AppInfo | null;
  onSaved: (s: Settings) => void;
}) {
  const [form, setForm] = useState<Settings>(settings);
  const [saved, setSaved] = useState("");
  const [update, setUpdate] = useState("");

  const set = <K extends keyof Settings>(k: K, v: Settings[K]) =>
    setForm((f) => ({ ...f, [k]: v }));

  const save = async () => {
    try {
      await api.saveSettings(form);
      onSaved(form);
      setSaved("Saved ✓");
      setTimeout(() => setSaved(""), 2000);
    } catch (e) {
      setSaved("Save failed: " + e);
    }
  };

  const checkUpdate = async () => {
    setUpdate("Checking…");
    try {
      setUpdate(await api.checkUpdate());
    } catch (e) {
      setUpdate("Failed: " + e);
    }
  };

  return (
    <Flex direction="column" gap="4" style={{ maxWidth: 640 }}>
      <Section title="Network">
        <Field label="Signaling server">
          <TextField.Root
            value={form.server}
            placeholder="ws://host:21116"
            onChange={(e) => set("server", e.target.value)}
          />
        </Field>
        <Field label="Reflect (srflx) address">
          <TextField.Root
            value={form.reflect}
            placeholder="host:21117"
            onChange={(e) => set("reflect", e.target.value)}
          />
        </Field>
      </Section>

      <Section title="Security">
        <Field label="Team secret">
          <TextField.Root
            type="password"
            value={form.secret}
            placeholder="shared secret"
            onChange={(e) => set("secret", e.target.value)}
          />
        </Field>
      </Section>

      <Section title="Connection defaults">
        <Field label="Local RDP listener (controller)">
          <TextField.Root
            value={form.default_listen}
            placeholder="127.0.0.1:33389"
            onChange={(e) => set("default_listen", e.target.value)}
          />
        </Field>
        <Field label="Local RDP service (host)">
          <TextField.Root
            value={form.default_rdp}
            placeholder="127.0.0.1:3389"
            onChange={(e) => set("default_rdp", e.target.value)}
          />
        </Field>
        <Toggle
          label="Force relay by default (skip P2P)"
          checked={form.force_relay}
          onChange={(v) => set("force_relay", v)}
        />
        <Toggle
          label="Enable UDP multitransport (P2P datagrams)"
          checked={form.enable_udp}
          onChange={(v) => set("enable_udp", v)}
        />
      </Section>

      <Section title="Appearance">
        <Field label="Theme">
          <Select.Root value={form.theme} onValueChange={(v) => set("theme", v)}>
            <Select.Trigger />
            <Select.Content>
              <Select.Item value="system">System</Select.Item>
              <Select.Item value="light">Light</Select.Item>
              <Select.Item value="dark">Dark</Select.Item>
            </Select.Content>
          </Select.Root>
        </Field>
      </Section>

      <Section title="Updates">
        <Toggle
          label="Check for updates on launch"
          checked={form.auto_check_updates}
          onChange={(v) => set("auto_check_updates", v)}
        />
        <Flex align="center" gap="3">
          <Button variant="soft" onClick={checkUpdate}>
            Check now
          </Button>
          <Text size="2" color="gray">
            {update}
          </Text>
        </Flex>
      </Section>

      <Section title="About">
        <Flex gap="5" wrap="wrap">
          <Text size="2" color="gray">
            Version <Badge variant="soft">{info?.version ?? "…"}</Badge>
          </Text>
          <Text size="2" color="gray">
            Device ID{" "}
            <Badge variant="soft" className="mono">
              {info?.device_id ?? "…"}
            </Badge>
          </Text>
        </Flex>
      </Section>

      <Flex align="center" gap="3">
        <Button onClick={save}>Save settings</Button>
        <Text size="2" color="gray">
          {saved}
        </Text>
      </Flex>
    </Flex>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <Card>
      <Heading size="3" mb="3">
        {title}
      </Heading>
      <Flex direction="column" gap="3">
        {children}
      </Flex>
    </Card>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label>
      <Text as="div" size="2" color="gray" mb="1">
        {label}
      </Text>
      {children}
    </label>
  );
}

function Toggle({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <Text as="label" size="2">
      <Flex align="center" gap="2">
        <Switch checked={checked} onCheckedChange={onChange} />
        {label}
      </Flex>
    </Text>
  );
}
