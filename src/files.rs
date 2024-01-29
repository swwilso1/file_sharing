//! The `files` module contains the `FileHandler` code that handles reading and writing files,
//! listing the files from the file system, sending the files to a peer, and hashing the files.

use crate::DynResult;

use futures::{AsyncReadExt, AsyncWriteExt};
use libp2p::Stream;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::AsyncReadExt as TokioAsyncReadExt;
use tokio::io::AsyncWriteExt as TokioAsyncWriteExt;

/// The number of bytes used for reading a slice from a file and from
/// the network streams.
const BUFFER_SIZE: usize = 1024;

/// A selector enum for the type of request to send to the peer.
enum TransmitRequest {
    /// Indicates that the request is for the file contents.
    FileContents,

    /// Indicates that the request is for the file digest.
    FileDigest,
}

/// The `FileHandler` struct encapsulates the code for handling files.  It provides methods for
/// listing the files in the directory, checking if a file exists, retrieving a file from a stream,
/// retrieving a digest from a stream, writing a file to a stream, writing a digest to a stream,
/// and removing a file from the file system.
pub struct FileHandler {
    /// The directory from which to serve the files.
    served_dir: PathBuf,

    /// A map of file names to file digests.
    digests: HashMap<String, String>,
}

impl FileHandler {
    /// The constructor for the [FileHandler] struct.
    ///
    /// # Arguments
    ///
    /// * `served_dir` - The directory from which to serve the files.
    pub fn new(served_dir: PathBuf) -> FileHandler {
        FileHandler {
            served_dir,
            digests: HashMap::new(),
        }
    }

    /// A simple getter for getting the served directory, this could also have been public
    /// in the [FileHandler] struct.  I tend to prefer getters/setters for the purposes of
    /// encapsulation, but I'm not sure if that's idiomatic Rust.
    pub fn get_served_dir(&self) -> PathBuf {
        self.served_dir.clone()
    }

    /// A simple method to read the files from the served directory and return them as a vector of
    /// names. The names are only the file names, not the full paths to the file on the file system.
    pub async fn list_files(&mut self) -> DynResult<Vec<String>> {
        let mut dir_files = tokio::fs::read_dir(&self.served_dir).await?;
        let mut files: Vec<PathBuf> = Vec::new();
        while let Some(file) = dir_files.next_entry().await? {
            if let Ok(meta) = file.metadata().await {
                if meta.is_file() {
                    files.push(file.path());
                }
            }
        }

        Ok(files
            .iter()
            .filter_map(|f| f.file_name().map(|f| f.to_string_lossy().to_string()))
            .collect::<Vec<String>>())
    }

    /// A method for checking if a file exists in the served directory.
    /// Returns true if the file exists, false otherwise.
    pub async fn file_exists(&self, file_name: &str) -> DynResult<bool> {
        let mut dir_files = tokio::fs::read_dir(&self.served_dir).await?;
        while let Some(file) = dir_files.next_entry().await? {
            if file.file_name().to_string_lossy() == file_name {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Function for reading the header sent on the file transfer stream. We overload the file transfer stream
    /// to send either the file contents or the file digest. We could have also handled this by supporting two
    /// different streams in the Swarm, but chose this route for time and simplicity.
    ///
    /// # Arguments
    ///
    /// * `stream` - The stream from which to read the header.
    async fn read_header(stream: &mut Stream) -> DynResult<(TransmitRequest, String)> {
        // Read 1 byte from the stream.
        // byte 1 is either 0 or 1.
        //  - 0 byte means send the file contents.
        //  - 1 byte means send the file digest.
        // We explicitly do not gracefully handle the case where byte 1 has some other value for time
        // purposes.

        let mut buf = [0u8; 1];

        let read = stream.read(&mut buf).await?;
        if read == 0 {
            return Err("No bytes read from stream".into());
        }

        let transmit_request = match buf[0] {
            0 => TransmitRequest::FileContents,
            1 => TransmitRequest::FileDigest,
            _ => return Err("Invalid transmit request".into()),
        };

        // The next 4 bytes are a big-endian 32-bit unsigned integer containing the length
        // of the file name. (bit-endian for network byte order).
        let mut buf = [0u8; 4];

        // Read the length from the sender.
        let read = stream.read(&mut buf).await?;
        if read == 0 {
            return Err("Did not read file name length from stream".into());
        }

        // Convert from network byte order.
        let file_name_length = u32::from_be_bytes(buf);

        // The next file_name_length bytes contain the name of the file.
        let mut bytes = vec![0u8; file_name_length as usize];
        let read = stream.read(&mut bytes).await?;
        if read == 0 {
            return Err("Did not read file name from stream".into());
        }

        let file_name = String::from_utf8(bytes)?;

        Ok((transmit_request, file_name))
    }

    /// A method for writing the header to the stream for sending either the file contents or the file
    /// digest.
    ///
    /// # Arguments
    ///
    /// * `request` - The type of request to send to the peer.
    /// * `file_name` - The name of the file to send to the peer.
    /// * `stream` - The stream to which to write the header.
    async fn write_header(
        request: TransmitRequest,
        file_name: &str,
        stream: &mut Stream,
    ) -> DynResult<()> {
        let mut buffer = Vec::<u8>::new();
        match request {
            TransmitRequest::FileContents => buffer.push(0),
            TransmitRequest::FileDigest => buffer.push(1),
        }

        let file_name_length = file_name.len() as u32;

        // Encode the length in network byte order.
        buffer.extend_from_slice(&file_name_length.to_be_bytes());
        buffer.extend_from_slice(file_name.as_bytes());

        stream.write_all(&buffer).await?;

        Ok(())
    }

    /// The peer requesting the file contacts the other peer and requests the file contents by sending the
    /// header of the file to get. Then whether for the contents or the digest, the peer waits for the remote
    /// peer to send the data or the digest. This code does not go to great lengths to ensure that files/digest
    /// correctly get transmitted/received in every scenario.

    /// A function for retrieving the file contents from the peer using a stream.
    ///
    /// # Arguments
    ///
    /// * `file_name` - The name of the file to retrieve from the peer.
    /// * `stream` - The stream from which to read the file contents.
    pub async fn retrieve_file_from_peer(
        &mut self,
        file_name: &str,
        stream: &mut Stream,
    ) -> DynResult<String> {
        // Send the header requesting the file contents from the peer.
        Self::write_header(TransmitRequest::FileContents, file_name, stream).await?;

        let file_path = self.served_dir.join(file_name);
        let mut file = File::create(file_path).await?;
        let mut hasher = Sha256::new();

        // Read the file contents and write them to disk.
        loop {
            let mut buffer = [0u8; BUFFER_SIZE];
            let bytes_read = stream.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }

            hasher.update(&buffer[..bytes_read]);
            file.write_all(&buffer[..bytes_read]).await?;
        }

        let hash_output = hasher.finalize();

        Ok(format!("{:x}", hash_output))
    }

    /// A function for reading the file digest for a file from a peer.
    ///
    /// # Arguments
    ///
    /// * `file_name` - The name of the file for which to retrieve the digest.
    /// * `stream` - The stream from which to read the file digest.
    pub async fn retrieve_digest_from_peer(
        &mut self,
        file_name: &str,
        stream: &mut Stream,
    ) -> DynResult<String> {
        // Send the header requesting the file digest
        Self::write_header(TransmitRequest::FileDigest, file_name, stream).await?;

        let mut bytes_to_go: usize = 64;
        let mut digest = Vec::<u8>::new();

        loop {
            let mut buffer = [0u8; 64];
            let bytes_read = stream.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }

            bytes_to_go -= bytes_read;
            digest.extend_from_slice(&buffer[..bytes_read]);
            if bytes_to_go == 0 {
                return Ok(String::from_utf8(digest)?);
            }
        }

        Err("Did not read file digest from stream".into())
    }

    /// Function for writing the file contents from the local machine to the remote peer.
    ///
    /// # Arguments
    ///
    /// * `stream` - The stream to which to write the file contents.
    /// * `file_name` - The name of the file to write to the stream.
    async fn send_file(&mut self, stream: &mut Stream, file_name: &str) -> DynResult<()> {
        let file_path = self.served_dir.join(file_name);
        if let Ok(mut file) = File::open(file_path).await {
            let mut buffer = [0u8; BUFFER_SIZE];

            let mut hasher = Sha256::new();

            loop {
                let mut take_handle = file.take(buffer.len() as u64);
                let bytes_read = take_handle.read(&mut buffer).await?;
                if bytes_read > 0 {
                    hasher.update(&buffer[..bytes_read]);
                    stream.write_all(&buffer[..bytes_read]).await?;
                } else {
                    break;
                }
                file = take_handle.into_inner();
            }

            let file_hash = hasher.finalize();
            self.digests
                .insert(file_name.to_string(), format!("{:x}", file_hash));
        }

        Ok(())
    }

    /// Function for sending the file digest to the remote peer.
    ///
    /// # Arguments
    ///
    /// * `stream` - The stream to which to write the file digest.
    /// * `file_name` - The name of the file for which to write the digest.
    async fn send_digest(&mut self, stream: &mut Stream, file_name: &str) -> DynResult<()> {
        if let Some(digest) = self.digests.get(file_name) {
            stream.write_all(digest.as_bytes()).await?;
        }
        Ok(())
    }

    /// The public function for sending the response to the peer when the peer requests either
    /// the file contents or the file digest.
    ///
    /// # Arguments
    ///
    /// * `stream` - The stream to which to write the response.
    pub async fn send_response(&mut self, stream: &mut Stream) -> DynResult<()> {
        let (transmit_request, file_name) = Self::read_header(stream).await?;
        match transmit_request {
            TransmitRequest::FileContents => self.send_file(stream, &file_name).await?,
            TransmitRequest::FileDigest => self.send_digest(stream, &file_name).await?,
        }
        Ok(())
    }

    /// Method for remove the file from the file system.  This function does not check to handle the case
    /// where the file may not exist.
    ///
    /// # Arguments
    ///
    /// * `file_name` - The name of the file to remove from the file system.
    pub async fn remove_file(&mut self, file_name: &str) -> DynResult<()> {
        let file_path = self.served_dir.join(file_name);
        tokio::fs::remove_file(file_path).await?;
        self.digests.remove(file_name);
        Ok(())
    }
}
