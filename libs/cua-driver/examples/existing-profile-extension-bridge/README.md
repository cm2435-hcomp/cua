# Existing-profile extension bridge spike

Disposable proof that an extension in an already-running Chrome profile can
mediate bounded CDP operations without relaunching or foregrounding Chrome.
This is experiment code, not a supported Cua Driver installation path.

## Install

1. Keep the existing Chrome process and profile running.
2. Open `chrome://extensions`, enable Developer mode, choose **Load unpacked**,
   and select the `extension/` directory beside this README.
3. Copy the resulting 32-character extension ID.
4. Install the user-scoped native host:

   ```sh
   python3 bridge.py install --extension-id EXTENSION_ID
   ```

5. Press **Reload** on the unpacked extension once so it connects to the newly
   installed native host.

Setup is explicitly user-visible. The steady-state proof below must not focus
Chrome or restart it.

## Probe

Open a deterministic local fixture in the Chrome window being tested and make
that tab active. Put another application in front, then run:

```sh
python3 bridge.py probe \
  --pid CHROME_PID \
  --window-id NATIVE_WINDOW_ID \
  --native-bounds LEFT,TOP,WIDTH,HEIGHT \
  --expected-origin http://127.0.0.1:PORT
```

The result contains only Chrome IDs, window geometry, timings, sizes, and
SHA-256 digests. DOM and screenshot payloads stay inside the extension and are
discarded after hashing.

To test lifecycle cleanup, run the same probe with a wait and close the approved
tab before the timeout:

```sh
python3 bridge.py probe \
  --pid CHROME_PID \
  --window-id NATIVE_WINDOW_ID \
  --native-bounds LEFT,TOP,WIDTH,HEIGHT \
  --expected-origin http://127.0.0.1:PORT \
  --wait-for-detach 30
```

The result should contain either a `detached` or `tab_closed` terminal event.
Navigating the attached tab must increment its document generation and refuse
operations carrying the old generation. Chrome builds that allow DevTools and
an extension debugger to coexist may keep both attached; if Chrome displaces
the extension, the same wait surfaces its `detached` event. Killing the native
host must detach the tab before reconnecting a fresh, authority-free channel.

## Local check and cleanup

```sh
python3 bridge.py self-check
python3 bridge.py uninstall
```

Remove the unpacked extension from `chrome://extensions` after the experiment.
