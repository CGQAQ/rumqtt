use crate::protocol::{Publish, PublishProperties};
use crate::Storage;
use bytes::Bytes;

impl Storage for Bytes {
    fn size(&self) -> usize {
        // For bytes len returns number of bytes in the given `Bytes`
        self.len()
    }
}

impl Storage for (Publish, Option<PublishProperties>) {
    fn size(&self) -> usize {
        4 + self.0.topic.len() + self.0.payload.len()
    }
}

impl Storage for Vec<u8> {
    fn size(&self) -> usize {
        // For bytes len returns number of bytes in the given `Bytes`
        self.len()
    }
}
