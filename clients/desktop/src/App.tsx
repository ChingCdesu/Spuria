import { useEffect, useMemo, useState } from "react";
import { Theme, Flex, Box, Tabs, Heading, Badge, Text } from "@radix-ui/themes";
import { api, type Settings as S, type AppInfo } from "./api";
import Home from "./components/Home";
import SettingsView from "./components/Settings";

function appearanceFor(theme: string): "light" | "dark" {
  if (theme === "light") return "light";
  if (theme === "dark") return "dark";
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export default function App() {
  const [settings, setSettings] = useState<S | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [tab, setTab] = useState("home");

  useEffect(() => {
    api.getSettings().then(setSettings).catch(console.error);
    api.getAppInfo().then(setInfo).catch(console.error);
  }, []);

  const appearance = useMemo(() => appearanceFor(settings?.theme ?? "system"), [settings?.theme]);

  return (
    <Theme appearance={appearance} accentColor="indigo" grayColor="slate" radius="large">
      <Flex direction="column" style={{ height: "100vh" }}>
        <Flex
          align="center"
          gap="4"
          px="4"
          py="2"
          style={{ borderBottom: "1px solid var(--gray-a4)" }}
        >
          <Heading size="5">Spuria</Heading>
          <Tabs.Root value={tab} onValueChange={setTab}>
            <Tabs.List>
              <Tabs.Trigger value="home">Home</Tabs.Trigger>
              <Tabs.Trigger value="settings">Settings</Tabs.Trigger>
            </Tabs.List>
          </Tabs.Root>
          <Box style={{ flexGrow: 1 }} />
          {info && (
            <Text size="2" color="gray">
              This device{" "}
              <Badge variant="soft" className="mono">
                {info.device_id}
              </Badge>
            </Text>
          )}
        </Flex>

        <Box p="5" style={{ flexGrow: 1, overflow: "auto" }}>
          {!settings ? (
            <Text color="gray">Loading…</Text>
          ) : (
            <>
              {/* Keep the runtime event subscriptions and session state alive while editing settings. */}
              <div hidden={tab !== "home"}>
                <Home
                  settings={settings}
                  deviceId={info?.device_id ?? "…"}
                  onOpenSettings={() => setTab("settings")}
                />
              </div>
              {tab === "settings" && (
                <SettingsView settings={settings} info={info} onSaved={setSettings} />
              )}
            </>
          )}
        </Box>
      </Flex>
    </Theme>
  );
}
