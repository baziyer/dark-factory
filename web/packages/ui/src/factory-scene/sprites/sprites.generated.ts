export const spriteSheet = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAIAAAABgCAYAAADVenpJAAAFX0lEQVR42u1drc4VMRBdf58AgcPgSBAkCBIkCZoAhvACKCRPAAqBxyDRmM8TNMFjkBheYEkvmS+9pd3O3+7svT2TNB8/e6bt9LTb3dOdb5oGs/nFk3mCjWsgAAzGscPhMOdlax/W+lt47grg0f+zjX+6+Ofr+ydFM4BaH9b6o/Ee5H/w9eNJ2dRHuvDR3dvzlzdP599X74/A9FPaAK0PLTZdQ9db2u+BtxIwMv7XDggsrdzDhxRLHaTOWttvbbvHAEbG/2gEttzLLD4k2HzW0Wyztt+C9xjA0PiXS9je8SUBottvHcDy/r0ZvhY8r00cx4el/rwObfs9+m8hUG3zJt3EtTaAbB8UBFq+8nurZAes8dHCSbCt0vNB92ot3nMCpcGyxl/to3yEkT7K5DNR4yPfxKUimb05+VJ59vLV9eAlXz0C0LKtxVsJ5BV/mu0qH3TxnXsPjz9TEMrSC4QXnnxIyVPDS4MvxVsJtJf4TwTO/1yWXgO88Ml61+8JbyXgHuI/lY3VLEGj460EjGw/zOk9PgagYpBjB48/CIAJCDun+xj0+AHPQ1j16Gi8R/Dfffh0Urb0Ya3f3P5L0OOtBBz5PMR/b7Wgx493HuJoUj2+nG3Q47c9D1Eu96b2S/XkkgBherZT8Mt76N7xJQHU9Vv06FyF0urZHnq4hUC14Hlt4jg+LPXndajb39KRuVp4a/DIJ0dO1eInxwMVKWBWPV7jo4WTYFtFfB6A9HjJ7M0PI6SSpEcaPIker8VbCTQ56fE04zzOQ3Bnb14PDXqKHw0+V46+CD1eQ6Ba/0c7D3HtBHr8uOchoMcPfp7hYnSMYQMA269BjgUBQACu/fn2eE4lyoe1/mi8y3006v5JnfcYQI0Pa/0tPHcF8Oy/avA99HCLHl8GYEsf1vqj8WYCeejx1gMNyE9gw1sJBD1+8PMQR4MeP3B+Aquebe0A8hME5Sfw0MMtBFr6vJreo2t85O/k16q/l59g7frNBKJ7hUVP9jpQUeYISP/268fnY+G0ofTBxS/lNejhe7kBOPjW5+XS9qsIlN83NHqyB4GmBT0+dZ4j7LTyE0jwtfwEPfxSfgIuvvV5ORdvJZCbHm85kFDT08sOSPV4LT7vP3cGRuGtBDpxtEc9vhzYLfB5/yWHWqLwVgKeOGn9XdsB6VtES/3IT6AnoJuOoO0ADPFfzSDHggAgANegxwfq8dHxhx4fqMdHxx96fKweHx1/6PHBejx+XwD0ePy+gAn5CWLjH/19P/ITBOUnWPq8WqLH1z7N5urh2vp7+QnWrt+DQEtyuiT+NSme3f5ajgCNHp/7kKhhrTz3HDVsKTcAV4/X4j0nUJkjQBP/3IdKDLLo6bV89Vo9Ph8Arh5ffh4u1eO1eCuBvOJfy08gkoOhx9v0eA2B9hJ/6PHBenx0/KGnB+vxyA9w5oYBWLC9ybGtDddWpZY/SFKoH1ocCHA4zDdu3lIVGsTa/+X5gJbwtfxBkpITgIthE2AEPZ4GMAWFM+j5dSUBpD56BOAQpCRAb8VhE2AUPX6tFYCL3yUBRtLjQQBHOfMc9XgQYHA9PpoA1rIaAabofPUb6fF7WgE0K8JqBAjLV++kh3MJBAI46tG9fPVcPd6qh0sIBAIwtGiJHr+UG4Crx2vxGgKNQgDaCLOfAmr56iV6fPl5uFSP1+KlBMIm8EL1eC6B8Bg4uB6PV8GD6/HRK4AWS2VVMWgEPT5aDvbaA2hxMNi/JdKiRUfjYQ4EsGCi8bALIcDz729nTgEBQAAQAAQAAUAAGAgA2xEBymNYrQICgAAgAAgAAoAAIAAIAALgKQCvgvdAAIhBMNiA9hc6zyHxeS9sNQAAAABJRU5ErkJggg==";
export const spriteSheetSize = {"width":128,"height":96} as const;
export const spriteAtlas = {
  "frame": 16,
  "sheet": "sprites.png",
  "frames": {
    "worker.claude_code.busy.0": {
      "x": 0,
      "y": 0
    },
    "worker.claude_code.busy.1": {
      "x": 16,
      "y": 0
    },
    "worker.claude_code.waiting.0": {
      "x": 32,
      "y": 0
    },
    "worker.claude_code.needs-you.0": {
      "x": 48,
      "y": 0
    },
    "worker.claude_code.idle.0": {
      "x": 64,
      "y": 0
    },
    "worker.claude_code.idle.1": {
      "x": 80,
      "y": 0
    },
    "worker.codex.busy.0": {
      "x": 96,
      "y": 0
    },
    "worker.codex.busy.1": {
      "x": 112,
      "y": 0
    },
    "worker.codex.waiting.0": {
      "x": 0,
      "y": 16
    },
    "worker.codex.needs-you.0": {
      "x": 16,
      "y": 16
    },
    "worker.codex.idle.0": {
      "x": 32,
      "y": 16
    },
    "worker.codex.idle.1": {
      "x": 48,
      "y": 16
    },
    "worker.shell.busy.0": {
      "x": 64,
      "y": 16
    },
    "worker.shell.busy.1": {
      "x": 80,
      "y": 16
    },
    "worker.shell.waiting.0": {
      "x": 96,
      "y": 16
    },
    "worker.shell.needs-you.0": {
      "x": 112,
      "y": 16
    },
    "worker.shell.idle.0": {
      "x": 0,
      "y": 32
    },
    "worker.shell.idle.1": {
      "x": 16,
      "y": 32
    },
    "overseer.claude_code.busy.0": {
      "x": 32,
      "y": 32
    },
    "overseer.claude_code.busy.1": {
      "x": 48,
      "y": 32
    },
    "overseer.claude_code.waiting.0": {
      "x": 64,
      "y": 32
    },
    "overseer.claude_code.needs-you.0": {
      "x": 80,
      "y": 32
    },
    "overseer.claude_code.idle.0": {
      "x": 96,
      "y": 32
    },
    "overseer.claude_code.idle.1": {
      "x": 112,
      "y": 32
    },
    "overseer.codex.busy.0": {
      "x": 0,
      "y": 48
    },
    "overseer.codex.busy.1": {
      "x": 16,
      "y": 48
    },
    "overseer.codex.waiting.0": {
      "x": 32,
      "y": 48
    },
    "overseer.codex.needs-you.0": {
      "x": 48,
      "y": 48
    },
    "overseer.codex.idle.0": {
      "x": 64,
      "y": 48
    },
    "overseer.codex.idle.1": {
      "x": 80,
      "y": 48
    },
    "overseer.shell.busy.0": {
      "x": 96,
      "y": 48
    },
    "overseer.shell.busy.1": {
      "x": 112,
      "y": 48
    },
    "overseer.shell.waiting.0": {
      "x": 0,
      "y": 64
    },
    "overseer.shell.needs-you.0": {
      "x": 16,
      "y": 64
    },
    "overseer.shell.idle.0": {
      "x": 32,
      "y": 64
    },
    "overseer.shell.idle.1": {
      "x": 48,
      "y": 64
    },
    "tile.floor.0": {
      "x": 64,
      "y": 64
    },
    "tile.floor.1": {
      "x": 80,
      "y": 64
    },
    "tile.wall": {
      "x": 96,
      "y": 64
    },
    "tile.door": {
      "x": 112,
      "y": 64
    },
    "bay.free": {
      "x": 0,
      "y": 80
    },
    "bay.staged": {
      "x": 16,
      "y": 80
    },
    "bay.ready": {
      "x": 32,
      "y": 80
    }
  }
} as const;
