use bytesize::ByteSize;
use mill_config::{AllocResources, NodeResources};

pub fn test_resources() -> NodeResources {
    NodeResources {
        cpu_total: 4.0,
        cpu_available: 4.0,
        memory_total: ByteSize::gib(8),
        memory_available: ByteSize::gib(8),
    }
}

pub fn test_alloc_resources() -> AllocResources {
    AllocResources { cpu: 1.0, memory: ByteSize::gib(1) }
}
