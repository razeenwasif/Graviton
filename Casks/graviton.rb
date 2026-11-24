cask "graviton" do
  version "0.2.0"
  sha256 "03dc13b5cc7829fe0bbf7197ffc17613def92a6114163904ef5f23388e4e1345"

  url "https://github.com/razeenwasif/Graviton/releases/download/v#{version}/Graviton-#{version}.dmg"
  name "Graviton"
  desc "Native Linux containers for Apple silicon Macs via Virtualization.framework"
  homepage "https://github.com/razeenwasif/Graviton"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on arch: :arm64
  depends_on macos: :sequoia

  app "Graviton.app"
  binary "#{appdir}/Graviton.app/Contents/MacOS/grav"

  uninstall launchctl: "dev.graviton.gravitond",
            quit:      "dev.graviton.mac"

  zap trash: [
    "~/Library/Application Support/Graviton",
    "~/Library/Caches/Graviton",
    "~/Library/LaunchAgents/dev.graviton.gravitond.plist",
    "~/Library/Preferences/dev.graviton.mac.plist",
  ]

  caveats <<~EOS
    To run gravitond automatically at login:
      grav service install

    To point standard `docker` CLI, Compose, and Dev Containers at Graviton:
      eval "$(grav docker-env)"

    To enable shell completions for `grav`:
      grav completions zsh > "${fpath[1]}/_grav"   # or bash / fish
  EOS
end
