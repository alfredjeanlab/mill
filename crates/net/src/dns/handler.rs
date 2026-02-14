use std::collections::HashMap;
use std::net::IpAddr;

use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{DNSClass, Name, RData, Record, RecordType};

/// Result of handling a DNS query.
pub enum QueryResult {
    /// A response to send back to the client.
    Response(Message),
    /// The query should be forwarded to an upstream resolver.
    Forward,
}

/// Handle a DNS query against the local `.mill.` record table.
///
/// - Names ending in `.mill.` are resolved locally (A records only).
/// - All other names return `Forward` to indicate upstream forwarding.
pub fn handle_query(msg: &Message, records: &HashMap<String, Vec<IpAddr>>) -> QueryResult {
    let query = match msg.queries().first() {
        Some(q) => q,
        None => {
            let mut resp = response_from(msg);
            resp.set_response_code(ResponseCode::FormErr);
            return QueryResult::Response(resp);
        }
    };

    let name = query.name().to_string();
    let record_type = query.query_type();

    // Check if this is a .mill. domain
    if !name.ends_with(".mill.") {
        return QueryResult::Forward;
    }

    let mut resp = response_from(msg);

    // Only handle A record queries
    if record_type != RecordType::A {
        resp.set_response_code(ResponseCode::NoError);
        return QueryResult::Response(resp);
    }

    // Strip the `.mill.` suffix to get the lookup key
    let lookup_key = name.trim_end_matches(".mill.").to_string();

    match records.get(&lookup_key) {
        Some(addrs) => {
            resp.set_response_code(ResponseCode::NoError);
            for addr in addrs {
                // Filter to IPv4 only (A records)
                if let IpAddr::V4(ipv4) = addr
                    && let Ok(rr_name) = Name::from_utf8(&name)
                {
                    let mut record = Record::from_rdata(rr_name, 5, RData::A(A(*ipv4)));
                    record.set_dns_class(DNSClass::IN);
                    resp.add_answer(record);
                }
            }
            QueryResult::Response(resp)
        }
        None => {
            resp.set_response_code(ResponseCode::NXDomain);
            QueryResult::Response(resp)
        }
    }
}

/// Build a response message from a query, copying the ID and queries.
pub(super) fn response_from(query: &Message) -> Message {
    let mut resp = Message::new();
    resp.set_id(query.id());
    resp.set_message_type(MessageType::Response);
    resp.set_recursion_desired(query.recursion_desired());
    resp.set_recursion_available(true);
    for q in query.queries() {
        resp.add_query(q.clone());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    use hickory_proto::op::Query;

    fn make_query(name: &str, rtype: RecordType) -> Message {
        let mut msg = Message::new();
        msg.set_id(1234);
        msg.set_message_type(MessageType::Query);
        let mut query = Query::new();
        let dns_name = Name::from_utf8(name).unwrap_or_else(|_| Name::root());
        query.set_name(dns_name);
        query.set_query_type(rtype);
        query.set_query_class(DNSClass::IN);
        msg.add_query(query);
        msg
    }

    fn test_records() -> HashMap<String, Vec<IpAddr>> {
        let mut records = HashMap::new();
        records.insert(
            "web".to_string(),
            vec![IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2))],
        );
        records.insert(
            "db".to_string(),
            vec![
                IpAddr::V4(Ipv4Addr::new(10, 99, 0, 10)),
                // IPv6 should be filtered out for A records
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ],
        );
        records
    }

    #[test]
    fn mill_a_query_returns_records() {
        let msg = make_query("web.mill.", RecordType::A);
        let records = test_records();
        match handle_query(&msg, &records) {
            QueryResult::Response(resp) => {
                assert_eq!(resp.response_code(), ResponseCode::NoError);
                assert_eq!(resp.answers().len(), 2);
            }
            QueryResult::Forward => panic!("expected response, got forward"),
        }
    }

    #[test]
    fn non_mill_query_returns_forward() {
        let msg = make_query("example.com.", RecordType::A);
        let records = test_records();
        assert!(matches!(handle_query(&msg, &records), QueryResult::Forward));
    }

    #[test]
    fn unknown_mill_name_returns_nxdomain() {
        let msg = make_query("unknown.mill.", RecordType::A);
        let records = test_records();
        match handle_query(&msg, &records) {
            QueryResult::Response(resp) => {
                assert_eq!(resp.response_code(), ResponseCode::NXDomain);
                assert!(resp.answers().is_empty());
            }
            QueryResult::Forward => panic!("expected nxdomain, got forward"),
        }
    }

    #[test]
    fn ipv6_addresses_filtered_from_a_records() {
        let msg = make_query("db.mill.", RecordType::A);
        let records = test_records();
        match handle_query(&msg, &records) {
            QueryResult::Response(resp) => {
                assert_eq!(resp.response_code(), ResponseCode::NoError);
                // Only the IPv4 address should be in the response
                assert_eq!(resp.answers().len(), 1);
            }
            QueryResult::Forward => panic!("expected response, got forward"),
        }
    }

    #[test]
    fn mill_aaaa_query_returns_empty() {
        let msg = make_query("web.mill.", RecordType::AAAA);
        let records = test_records();
        match handle_query(&msg, &records) {
            QueryResult::Response(resp) => {
                assert_eq!(resp.response_code(), ResponseCode::NoError);
                assert!(resp.answers().is_empty());
            }
            QueryResult::Forward => panic!("expected response, got forward"),
        }
    }
}
