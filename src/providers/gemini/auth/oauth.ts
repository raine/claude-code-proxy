import { createServer } from "node:http"
import { constants as fsConstants, promises as fs } from "node:fs"
import { dirname, join, resolve } from "node:path"
import { homedir } from "node:os"
import net from "node:net"
import { OAuth2Client, type Credentials } from "google-auth-library"
import { geminiOauthCredsPath } from "../../../config.ts"

const OAUTH_SCOPE = [
  "https://www.googleapis.com/auth/cloud-platform",
  "https://www.googleapis.com/auth/userinfo.email",
  "https://www.googleapis.com/auth/userinfo.profile",
]

export function authPath(): string {
  return expandHome(geminiOauthCredsPath() ?? "~/.gemini/oauth_creds.json")
}

export async function loadCredentials(): Promise<Credentials | undefined> {
  try {
    return JSON.parse(await fs.readFile(authPath(), "utf8")) as Credentials
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return undefined
    throw err
  }
}

export async function saveCredentials(credentials: Credentials): Promise<void> {
  const path = authPath()
  await fs.mkdir(dirname(path), { recursive: true })
  await fs.writeFile(path, JSON.stringify(credentials, null, 2), { mode: 0o600 })
  await fs.chmod(path, 0o600).catch(() => {})
}

export async function clearCredentials(): Promise<void> {
  await fs.unlink(authPath()).catch((err) => {
    if ((err as NodeJS.ErrnoException).code !== "ENOENT") throw err
  })
}

export async function authClient(): Promise<OAuth2Client> {
  const credentials = await loadCredentials()
  if (!credentials) {
    throw new GeminiAuthError(
      `Not authenticated. Run: claude-code-proxy gemini auth login. Expected credentials at ${authPath()}`,
    )
  }
  const oauthClient = await loadOauthClientConfig()

  const client = new OAuth2Client({
    clientId: oauthClient.clientId,
    clientSecret: oauthClient.clientSecret,
  })
  client.setCredentials(credentials)
  client.on("tokens", async (tokens) => {
    await saveCredentials({
      ...client.credentials,
      ...tokens,
      refresh_token: tokens.refresh_token ?? client.credentials.refresh_token,
    })
  })
  return client
}

export async function accessToken(): Promise<string> {
  const client = await authClient()
  const token = await client.getAccessToken()
  if (!token.token) {
    throw new GeminiAuthError("Gemini OAuth credentials did not produce an access token")
  }
  return token.token
}

export async function runBrowserLogin(): Promise<void> {
  const oauthClient = await loadOauthClientConfig()
  const port = await getAvailablePort()
  const redirectUri = `http://127.0.0.1:${port}/oauth2callback`
  const client = new OAuth2Client({
    clientId: oauthClient.clientId,
    clientSecret: oauthClient.clientSecret,
    redirectUri,
  })
  const state = crypto.randomUUID()
  const authUrl = client.generateAuthUrl({
    redirect_uri: redirectUri,
    access_type: "offline",
    scope: OAUTH_SCOPE,
    state,
    prompt: "consent",
  })

  console.log("Opening Google OAuth in your browser.")
  console.log(authUrl)

  const callback = waitForOAuthCallback(port, state, redirectUri, client)
  await openBrowser(authUrl).catch(() => {})
  const tokens = await callback
  await saveCredentials(tokens)
  console.log(`Auth saved in ${authPath()}`)
}

export async function printStatus(): Promise<void> {
  const credentials = await loadCredentials()
  if (!credentials) {
    console.log("Not authenticated")
    console.log(`Storage: ${authPath()}`)
    process.exit(1)
  }
  const client = await authClient()
  const token = await client.getAccessToken()
  if (!token.token) {
    console.log("Not authenticated")
    console.log(`Storage: ${authPath()}`)
    process.exit(1)
  }
  const expiry = client.credentials.expiry_date ?? credentials.expiry_date
  console.log("Authenticated")
  if (expiry) {
    const seconds = Math.floor((expiry - Date.now()) / 1000)
    console.log(`Expires: ${new Date(expiry).toISOString()} (in ${seconds}s)`)
  }
  console.log(`Scope: ${client.credentials.scope ?? credentials.scope ?? "(none)"}`)
  console.log(`Storage: ${authPath()}`)
}

async function waitForOAuthCallback(
  port: number,
  state: string,
  redirectUri: string,
  client: OAuth2Client,
): Promise<Credentials> {
  return new Promise((resolve, reject) => {
    const server = createServer(async (req, res) => {
      try {
        const url = new URL(req.url ?? "/", `http://127.0.0.1:${port}`)
        if (url.pathname !== "/oauth2callback") {
          res.writeHead(404)
          res.end("Not found")
          return
        }
        if (url.searchParams.get("state") !== state) {
          throw new Error("OAuth state mismatch")
        }
        const code = url.searchParams.get("code")
        if (!code) {
          throw new Error(url.searchParams.get("error_description") ?? "No OAuth code returned")
        }
        const { tokens } = await client.getToken({ code, redirect_uri: redirectUri })
        client.setCredentials(tokens)
        res.writeHead(200, { "content-type": "text/plain" })
        res.end("Authentication succeeded. You can close this tab.")
        resolve(tokens)
      } catch (err) {
        res.writeHead(500, { "content-type": "text/plain" })
        res.end(`Authentication failed: ${String(err)}`)
        reject(err)
      } finally {
        server.close()
      }
    })
    server.on("error", reject)
    server.listen(port, "127.0.0.1")
  })
}

async function openBrowser(url: string): Promise<void> {
  const command =
    process.platform === "darwin"
      ? ["open", url]
      : process.platform === "win32"
        ? ["cmd", "/c", "start", "", url]
        : ["xdg-open", url]
  const child = Bun.spawn(command, { stdout: "ignore", stderr: "ignore" })
  await child.exited
}

function getAvailablePort(): Promise<number> {
  const fromEnv = process.env.OAUTH_CALLBACK_PORT
  if (fromEnv) {
    const port = Number(fromEnv)
    if (Number.isInteger(port) && port > 0 && port < 65536) return Promise.resolve(port)
  }

  return new Promise((resolve, reject) => {
    const server = net.createServer()
    server.listen(0, "127.0.0.1")
    server.on("listening", () => {
      const addr = server.address()
      if (!addr || typeof addr === "string") {
        reject(new Error("Could not allocate OAuth callback port"))
        return
      }
      const port = addr.port
      server.close(() => resolve(port))
    })
    server.on("error", reject)
  })
}

function expandHome(path: string): string {
  if (path === "~") return homedir()
  if (path.startsWith("~/")) return join(homedir(), path.slice(2))
  return path
}

interface OAuthClientConfig {
  clientId: string
  clientSecret: string
}

async function loadOauthClientConfig(): Promise<OAuthClientConfig> {
  const fromEnv = {
    clientId: process.env.CCP_GEMINI_OAUTH_CLIENT_ID,
    clientSecret: process.env.CCP_GEMINI_OAUTH_CLIENT_SECRET,
  }
  if (fromEnv.clientId && fromEnv.clientSecret) return fromEnv as OAuthClientConfig

  const fromCli = await readOauthClientConfigFromGeminiCli()
  if (fromCli) return fromCli

  throw new GeminiAuthError(
    "Could not locate Gemini CLI OAuth client config. Install Gemini CLI or set CCP_GEMINI_OAUTH_CLIENT_ID and CCP_GEMINI_OAUTH_CLIENT_SECRET.",
  )
}

async function readOauthClientConfigFromGeminiCli(): Promise<OAuthClientConfig | undefined> {
  const dirs = await geminiCliBundleDirs()
  for (const dir of dirs) {
    let files: string[]
    try {
      files = await fs.readdir(dir)
    } catch {
      continue
    }
    for (const file of files) {
      if (!/^chunk-.*\.js$/.test(file)) continue
      const text = await fs.readFile(join(dir, file), "utf8").catch(() => "")
      const clientId = text.match(/OAUTH_CLIENT_ID\s*=\s*"([^"]+)"/)?.[1]
      const clientSecret = text.match(/OAUTH_CLIENT_SECRET\s*=\s*"([^"]+)"/)?.[1]
      if (clientId && clientSecret) return { clientId, clientSecret }
    }
  }
  return undefined
}

async function geminiCliBundleDirs(): Promise<string[]> {
  const dirs = new Set<string>()
  const bin = await findOnPath("gemini")
  if (bin) {
    const realBin = await fs.realpath(bin).catch(() => bin)
    dirs.add(resolve(dirname(realBin), "..", "libexec", "lib", "node_modules", "@google", "gemini-cli", "bundle"))
  }

  for (const prefix of ["/opt/homebrew", "/usr/local"]) {
    const cellar = join(prefix, "Cellar", "gemini-cli")
    const versions = await fs.readdir(cellar).catch(() => [])
    for (const version of versions) {
      dirs.add(join(cellar, version, "libexec", "lib", "node_modules", "@google", "gemini-cli", "bundle"))
    }
  }

  return [...dirs]
}

async function findOnPath(name: string): Promise<string | undefined> {
  const path = process.env.PATH ?? ""
  for (const dir of path.split(":")) {
    if (!dir) continue
    const candidate = join(dir, name)
    try {
      await fs.access(candidate, fsConstants.X_OK)
      return candidate
    } catch {
    }
  }
  return undefined
}

export class GeminiAuthError extends Error {
  constructor(message: string) {
    super(message)
    this.name = "GeminiAuthError"
  }
}
