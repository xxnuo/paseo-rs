/* oxlint-disable react-perf/jsx-no-new-function-as-prop -- NSH components execute once, without React re-renders. */
import { mount, signal, Text, View } from "nsh";
import { Button, Input } from "nsh/base";
import { encodeTerminalPaste } from "../../app/src/terminal/runtime/terminal-paste";

// A feasibility probe, not a terminal: verifies upstream logic in the native VM.
function RenderProbe() {
  const draft = signal("");
  const result = signal("");
  return (
    <View class="flex-1 bg-[#18181b] p-4 gap-3">
      <Text class="text-[#f4f4f5]">Paseo rendering probe</Text>
      <Input
        accessibilityLabel="Paste text"
        value={draft()}
        onChange={({ value }) => draft.set(value)}
        class="border border-[#3f3f46] text-[#f4f4f5] p-2"
      />
      <Button
        accessibilityLabel="Encode paste"
        disabled={!draft()}
        onPress={() => result.set(encodeTerminalPaste({ text: draft(), bracketedPaste: true }))}
      >
        <Text class="text-[#f4f4f5]">Encode paste</Text>
      </Button>
      <Text testID="encoded" class="text-[#a1a1aa]">
        {result()}
      </Text>
    </View>
  );
}

mount(RenderProbe);
