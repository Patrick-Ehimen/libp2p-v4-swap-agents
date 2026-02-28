import "dotenv/config";
import express from "express";
import { Synapse, calibration } from "@filoz/synapse-sdk";
import { privateKeyToAccount } from "viem/accounts";

const PORT = process.env.SIDECAR_PORT || 3001;
// Accept key with or without 0x prefix (matches Ethereum .env convention)
const RAW_KEY = process.env.FILECOIN_PRIVATE_KEY;
const PRIVATE_KEY = RAW_KEY
  ? RAW_KEY.startsWith("0x") ? RAW_KEY : `0x${RAW_KEY}`
  : null;

const app = express();
app.use(express.json({ limit: "10mb" }));

let synapse = null;

function initSynapse() {
  if (!PRIVATE_KEY) {
    console.warn("[sidecar] FILECOIN_PRIVATE_KEY not set — uploads will fail");
    return;
  }
  try {
    const account = privateKeyToAccount(PRIVATE_KEY);
    synapse = Synapse.create({ chain: calibration, account });
    console.log(
      `[sidecar] Synapse SDK initialized (Calibration testnet, account: ${account.address})`
    );
  } catch (err) {
    console.error("[sidecar] Failed to initialize Synapse SDK:", err.message);
  }
}

app.get("/health", (_req, res) => {
  res.json({
    status: "ok",
    configured: synapse !== null,
  });
});

app.post("/upload", async (req, res) => {
  if (!synapse) {
    return res
      .status(503)
      .json({ error: "Synapse SDK not initialized — check FILECOIN_PRIVATE_KEY" });
  }

  const body = req.body;
  if (!body || (typeof body === "object" && Object.keys(body).length === 0)) {
    return res.status(400).json({ error: "Empty request body" });
  }

  try {
    const payload = typeof body === "string" ? body : JSON.stringify(body);
    // Synapse SDK requires minimum 127 bytes; pad small payloads
    const padded = payload.length < 127 ? payload.padEnd(127) : payload;
    const buffer = new TextEncoder().encode(padded);
    const result = await synapse.storage.upload(new Uint8Array(buffer));

    console.log(`[sidecar] Uploaded ${buffer.length} bytes → PieceCID: ${result.pieceCid}`);
    res.json({ pieceCid: result.pieceCid });
  } catch (err) {
    // Extract concise "Details:" line from verbose RPC errors
    const details = err.message.match(/Details:\s*(.+)/)?.[1];
    const short = details || err.message.split("\n")[0];
    console.error("[sidecar] Upload failed:", short);
    res.status(500).json({ error: short });
  }
});

app.listen(PORT, () => {
  console.log(`[sidecar] Listening on http://localhost:${PORT}`);
  initSynapse();
});
