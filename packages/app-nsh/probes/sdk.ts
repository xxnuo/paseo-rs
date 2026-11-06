import { createPaseoClient } from "@getpaseo/client";
import { WSOutboundMessageSchema } from "@getpaseo/protocol/messages";
import { validateWSOutboundMessage } from "@getpaseo/protocol/validation/ws-outbound";
import { decrypt, deriveSharedKey, encrypt, generateKeyPair } from "@getpaseo/relay";

// No connection is opened: native WebSocket I/O is a separate probe.
export async function probeSdk() {
  const client = createPaseoClient({
    url: "ws://127.0.0.1:6767/ws",
    reconnect: { enabled: false },
  });
  try {
    const initialState = client.getConnectionState();
    const agentId = client.agents.ref("nsh-probe-agent").id;
    const workspaceId = client.workspaces.ref("nsh-probe-workspace").id;

    const alice = generateKeyPair();
    const bob = generateKeyPair();
    const senderKey = deriveSharedKey(alice.secretKey, bob.publicKey);
    const receiverKey = deriveSharedKey(bob.secretKey, alice.publicKey);
    const ciphertext = encrypt(senderKey, "Paseo SDK / 中文 / nsh");
    const plaintext = new TextDecoder().decode(decrypt(receiverKey, ciphertext));

    const pong = WSOutboundMessageSchema.parse({ type: "pong" });
    const invalidPong = { type: "not-a-paseo-message" };
    return {
      initialState,
      agentId,
      workspaceId,
      plaintext,
      pong,
      invalidSchemaAccepted: WSOutboundMessageSchema.safeParse(invalidPong).success,
      validatorAccepted: validateWSOutboundMessage(pong).success,
      invalidValidatorAccepted: validateWSOutboundMessage(invalidPong).success,
    };
  } finally {
    await client.close();
  }
}
