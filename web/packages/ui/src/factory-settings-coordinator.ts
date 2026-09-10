import {
  CAPABILITIES,
  type BrowserSession,
  type DiscoveredAccountView,
} from "@dark-factory/client";

const LOOPBACK_GRANT = CAPABILITIES.human_actions | CAPABILITIES.terminal_input;

export type FactoryRemoteInvite = Readonly<{
  link: string;
  svg: string;
  expiresAtMs: bigint;
}>;

type SettingsSession = Pick<BrowserSession, "discoverAccounts" | "linkAccount" | "inviteRemote" | "capabilities">;

type SettingsOwner = Readonly<{
  session(): SettingsSession | undefined;
  ready(): boolean;
  generation(): number;
  current(generation: number): boolean;
  errorCode(error: unknown): string;
  publish(): void;
}>;

/** Owns SETTINGS-only observations and remote invitation state. */
export class FactorySettingsCoordinator {
  readonly #owner: SettingsOwner;
  #remoteInvite: FactoryRemoteInvite | undefined;
  #remoteInviteError: string | undefined;
  #remoteInvitePending = false;
  #accounts: readonly DiscoveredAccountView[] | undefined;
  #accountsPending = false;
  #accountsError: string | undefined;

  constructor(owner: SettingsOwner) {
    this.#owner = owner;
  }

  get remoteInvite(): FactoryRemoteInvite | undefined { return this.#remoteInvite; }
  get remoteInviteError(): string | undefined { return this.#remoteInviteError; }
  get remoteInviteAllowed(): boolean {
    return this.#owner.ready() && ((this.#owner.session()?.capabilities ?? 0) & LOOPBACK_GRANT) === LOOPBACK_GRANT;
  }
  get accounts(): readonly DiscoveredAccountView[] | undefined { return this.#accounts; }
  get accountsPending(): boolean { return this.#accountsPending; }
  get accountsError(): string | undefined { return this.#accountsError; }

  clearRemoteInvite(): void {
    this.#remoteInvite = undefined;
    this.#remoteInviteError = undefined;
  }

  async loadAccounts(): Promise<void> {
    const session = this.#owner.session();
    if (!this.#owner.ready() || session === undefined || this.#accountsPending) return;
    const generation = this.#owner.generation();
    this.#accountsPending = true;
    this.#owner.publish();
    try {
      this.#accounts = await session.discoverAccounts();
      if (!this.#owner.current(generation)) return;
      this.#accountsError = undefined;
    } catch (error) {
      if (!this.#owner.current(generation)) return;
      this.#accountsError = this.#owner.errorCode(error);
    } finally {
      if (this.#owner.current(generation)) this.#accountsPending = false;
    }
    this.#owner.publish();
  }

  async linkAccount(request: { provider: "claude_code" | "codex"; home: string; label: string }): Promise<void> {
    const session = this.#owner.session();
    if (!this.#owner.ready() || session === undefined || this.#accountsPending) return;
    const generation = this.#owner.generation();
    this.#accountsPending = true;
    this.#owner.publish();
    try {
      await session.linkAccount(request);
      if (!this.#owner.current(generation)) return;
      this.#accountsError = undefined;
    } catch (error) {
      if (!this.#owner.current(generation)) return;
      this.#accountsError = this.#owner.errorCode(error);
      this.#accountsPending = false;
      this.#owner.publish();
      return;
    } finally {
      if (this.#owner.current(generation)) this.#accountsPending = false;
    }
    this.#owner.publish();
    await this.loadAccounts();
  }

  async inviteRemote(): Promise<void> {
    const session = this.#owner.session();
    if (!this.#owner.ready() || session === undefined || this.#remoteInvitePending) return;
    const generation = this.#owner.generation();
    this.#remoteInvitePending = true;
    try {
      const invite = await session.inviteRemote();
      if (!this.#owner.current(generation)) return;
      this.#remoteInvite = { link: invite.link, svg: invite.svg, expiresAtMs: invite.expiresAtMs };
      this.#remoteInviteError = undefined;
    } catch (error) {
      if (!this.#owner.current(generation)) return;
      this.#remoteInvite = undefined;
      this.#remoteInviteError = this.#owner.errorCode(error);
    } finally {
      this.#remoteInvitePending = false;
    }
    this.#owner.publish();
  }

  dismissRemoteInvite(): void {
    this.#remoteInvite = undefined;
    this.#remoteInviteError = undefined;
    this.#owner.publish();
  }
}
