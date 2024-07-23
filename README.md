# Distributed Systems Engineering Assignment Solution

## Introduction

This application exists to meet the requirements of the libp2p-file-sharing
assignment.

The assignment requested the following things:

- [x] A command line application that can be used to interactively share files over a local network.
- [x] A user `A` can request available files from a peer `B`.
- [x] The user can interactively select a file to download from the peer from the list of files.
- [x] The application should support large files (100Gb) without excessive RAM usage.
- [x] The application should check the integrity of the file.

## Design

This application provides a single program that can run in either a file serving mode,
or a client mode.

### File Serving Mode

In file serving mode, the program will start a libp2p node that will listen for
connections.  The node advertises a request-response protocol and a stream
protocol. In addition, the node advertises its presence using a mDNS service announcement.

#### Request-Response

The application uses the request-response protocol to allow one peer to
query a remote peer for a list of files that it has available for download. In addition,
the local peer can request whether a file exists on the remote peer.

#### Stream

The application provides a stream protocol that allows a local peer to request a file and the file's
hash digest from a remote peer. The server will transmit the file contents and the file digest over
the stream.

#### Shutdown

Just use Ctrl-C to shut down the application when in file serving mode.

### Client Mode

#### REPL 

The application provides a very basic REPL (read-eval-print-loop) service that allows
a user to accomplish the requirements of the assignment. The REPL supports the following
commands:

- `whoami` - display the local peer id.
- `peers` - display the list of peers that have been discovered via mDNS.
- `dial <peerid>` - connect to a peer using a peer id.
- `list` - list the files on the local file system.
- `list <peerid>` - list the files available on a remote peer.
- `get <peerid> <filename>` - download a file from a remote peer.

In addition, the REPL also supports:

- `help` - display a list of commands.
- `quit` - exit the application.

#### File Sharing

When running in client mode, the application is also able to serve files to the peer
network. Ie, in client mode the application runs both the client and the server.  A more
clean implementation might completely separate these two concerns, but for this exercise,
it was simpler to keep them running together while in client mode.

## Implementation

- The application is implemented in Rust, using the [libp2p](https://libp2p.io/) library.
- As requested, the application takes care not to load a file's full contents into memory prior to
transmitting it over the network. Instead, the application reads and transmits the file in smaller
chunks to limit memory usage.
- As requested, the application will calculate a hash digest on the contents of the file both on
the server side and the client side. The server will send the hash digest to the client, and
the client will compare the hash digest of the received file with the hash digest for the file contents
it received and will not save the file unless the hash digests match.

## Assumptions

For the most part, various assumptions are documented in the comments in the code itself. Here are
a few additional assumptions that are not documented in the code:

- In order to not spend too much time on the exercise, function call results are not always checked
to ensure success. I extended a reasonable effort to handle errors and error conditions, however, this
is not a production application so some cases may not be handled.
- This code is probably not performant under significant peer load. The server code that handles connections
for the stream portion does not attempt to parallelize the handling of multiple connections.  This could lead
to poor performance under load in a production environment.
- The communication protocol is not as rigorous as it could be when verifying that files exist and
can be downloaded.  The application attempts to verify file availability, but a malicious peer could cause problems.
The protocol is engineered for the time constraints, not production level security.
