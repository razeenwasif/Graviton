cask "graviton" do
  version "0.1.0"
  sha256 "0dc684483b2bdac1cf795d4db15cae1a47482fbf6ca2f79a4ab7c1afc9746eb9"

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
