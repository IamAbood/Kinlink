# Code signing policy

Windows builds of Kinlink are built by GitHub Actions directly from the source in this
repository (see `.github/workflows/build.yml`) and signed only from those builds.

Free code signing provided by [SignPath.io](https://signpath.io), certificate by
[SignPath Foundation](https://signpath.org). *(Applies once the project is approved.)*

## Team roles

- Committers and reviewers: [IamAbood](https://github.com/IamAbood)
- Approvers (who may release a signed build): [IamAbood](https://github.com/IamAbood)

## Privacy

Kinlink does not send information to any server run by its authors. Friends find each other
through public MQTT brokers (broker.emqx.io, broker.hivemq.com) using end-to-end encrypted,
anonymous messages; video, input, audio and files go directly between the two PCs, encrypted.
The auto-updater only reads this repository's public GitHub Releases. The optional network test
uses Cloudflare's public speed-test endpoints. Nothing is collected about you.
