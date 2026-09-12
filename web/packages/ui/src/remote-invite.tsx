import { useEffect, useState } from "react";
import { CAPABILITIES, type BrowserClientsView } from "@dark-factory/client";
import type { FactoryRemoteInvite } from "./factory-app-controller.js";

export type RemoteInvitePanelProps = {
  invite?: FactoryRemoteInvite;
  error?: string;
  onInvite?: () => void;
  onDismiss?: () => void;
  /** The identities the factory has granted; asked for when the panel mounts. */
  devices?: BrowserClientsView;
  devicesError?: string;
  ownClientId?: string;
  onLoadDevices?: () => void;
  onRevokeDevice?: (device: { clientId: string; expectedRevision: bigint }) => void;
};

/** Pairing a phone from the console itself: one button, then the code it
 * mints. The link is shown as text, not an anchor: opening it here would pair
 * this desktop as a remote client and consume the phone's one-shot challenge.
 * Below it, every identity the factory has granted, with REVOKE for each
 * other than this console's own. */
export function RemoteInvitePanel({ invite, error, onInvite, onDismiss, devices, devicesError, ownClientId, onLoadDevices, onRevokeDevice }: RemoteInvitePanelProps) {
  const [confirming, setConfirming] = useState<string | undefined>(undefined);
  useEffect(() => { onLoadDevices?.(); }, []);
  return (
    <section className="dfFactoryConsole__section dfFactoryConsole__pairPhone" aria-label="PAIR A PHONE">
      {invite === undefined ? (
        <button type="button" disabled={onInvite === undefined} onClick={onInvite}>PAIR A PHONE</button>
      ) : (
        <>
          <img alt="QR code: scan with your phone" src={`data:image/svg+xml;utf8,${encodeURIComponent(invite.svg)}`} />
          <code>{invite.link}</code>
          <p>VALID FOR FIVE MINUTES</p>
        </>
      )}
      {error === undefined ? null : (
        <p className="dfFactoryConsole__empty" role="alert">NO PAIRING CODE — {error.replace(/_/g, " ").toUpperCase()}</p>
      )}
      {invite === undefined && error === undefined ? null : (
        <button type="button" disabled={onDismiss === undefined} onClick={onDismiss}>DISMISS</button>
      )}
      <div className="dfFactoryConsole__sectionHeading">
        <h2>PAIRED DEVICES</h2>
        <span>{devices === undefined ? "—" : `${devices.clients.length}${devices.more ? "+" : ""}`}</span>
      </div>
      {devicesError === undefined ? null : (
        <p className="dfFactoryConsole__empty" role="alert">DEVICES — {devicesError.replace(/_/g, " ").toUpperCase()}</p>
      )}
      {devices === undefined ? <p className="dfFactoryConsole__empty">asking the factory</p> : devices.clients.length === 0 ? <p className="dfFactoryConsole__empty">nothing paired</p> : (
        <ul className="dfFactoryConsole__devices">
          {devices.clients.map((device) => {
            const own = device.clientId === ownClientId;
            // A grant without terminal input is a phone; the loopback grant is a browser on this machine.
            const kind = (device.capabilities & CAPABILITIES.terminal_input) === 0 ? "PHONE" : "BROWSER";
            return (
              <li key={device.clientId} className="dfFactoryConsole__device">
                <span>{kind}{own ? " · THIS BROWSER" : ""}</span>
                <span className="dfFactoryConsole__deviceSince">{new Date(Number(device.createdAtMs)).toLocaleDateString()}</span>
                {own || onRevokeDevice === undefined ? null : confirming === device.clientId ? (
                  <>
                    <button type="button" onClick={() => { setConfirming(undefined); onRevokeDevice({ clientId: device.clientId, expectedRevision: device.revision }); }}>CONFIRM REVOKE</button>
                    <button type="button" onClick={() => setConfirming(undefined)}>KEEP</button>
                  </>
                ) : (
                  <button type="button" onClick={() => setConfirming(device.clientId)}>REVOKE</button>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
