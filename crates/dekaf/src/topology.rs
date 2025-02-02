use anyhow::Context;
use gazette::{broker, journal, uuid};
use proto_flow::flow;

/// Fetch the names of all collections which the current user may read.
/// Each is mapped into a kafka topic.
pub async fn fetch_all_collection_names(
    client: &postgrest::Postgrest,
) -> anyhow::Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Row {
        catalog_name: String,
    }
    let rows: Vec<Row> = client
        .from("live_specs_ext")
        .eq("spec_type", "collection")
        .select("catalog_name")
        .execute()
        .await
        .and_then(|r| r.error_for_status())
        .context("listing current catalog specifications")?
        .json()
        .await?;

    Ok(rows
        .into_iter()
        .map(|Row { catalog_name }| catalog_name)
        .collect())
}

/// Collection is the assembled metadata of a collection being accessed as a Kafka topic.
pub struct Collection {
    pub journal_client: journal::Client,
    pub key_ptr: Vec<doc::Pointer>,
    pub key_schema: avro::Schema,
    pub not_before: uuid::Clock,
    pub partitions: Vec<Partition>,
    pub spec: flow::CollectionSpec,
    pub uuid_ptr: doc::Pointer,
    pub value_schema: avro::Schema,
}

/// Partition is a collection journal which is mapped into a stable Kafka partition order.
pub struct Partition {
    pub create_revision: i64,
    pub spec: broker::JournalSpec,
    pub _mod_revision: i64,
    pub _route: broker::Route,
}

impl Collection {
    /// Build a Collection by fetching its spec, a authenticated data-plane access token, and its partitions.
    pub async fn new(
        client: &postgrest::Postgrest,
        collection: &str,
    ) -> anyhow::Result<Option<Self>> {
        let not_before = uuid::Clock::default();

        // Build a journal client and use it to fetch partitions while concurrently
        // fetching the collection's metadata from the control plane.
        let client_partitions = async {
            let journal_client = Self::build_journal_client(&client, collection).await?;
            let partitions = Self::fetch_partitions(&journal_client, collection).await?;
            Ok((journal_client, partitions))
        };
        let (spec, client_partitions): (anyhow::Result<_>, anyhow::Result<_>) =
            futures::join!(Self::fetch_spec(&client, collection), client_partitions);

        let Some(spec) = spec? else { return Ok(None) };
        let (journal_client, partitions) = client_partitions?;

        let key_ptr: Vec<doc::Pointer> =
            spec.key.iter().map(|p| doc::Pointer::from_str(p)).collect();
        let uuid_ptr = doc::Pointer::from_str(&spec.uuid_ptr);

        let json_schema = if spec.read_schema_json.is_empty() {
            &spec.write_schema_json
        } else {
            &spec.read_schema_json
        };
        let (key_schema, value_schema) = avro::json_schema_to_avro(json_schema, &key_ptr)?;

        tracing::debug!(
            collection,
            partitions = partitions.len(),
            "built collection"
        );

        Ok(Some(Self {
            journal_client,
            key_ptr,
            key_schema,
            not_before,
            partitions,
            spec,
            uuid_ptr,
            value_schema,
        }))
    }

    /// Map the collection's key and value Avro schema into globally unique registry IDs.
    /// This will content-address each schema to fetch a current registry ID if one is available,
    /// or will register a new schema if not.
    pub async fn registered_schema_ids(
        &self,
        client: &postgrest::Postgrest,
    ) -> anyhow::Result<(u32, u32)> {
        let (key_id, value_id) = futures::try_join!(
            Self::registered_schema_id(client, &self.spec.name, &self.key_schema),
            Self::registered_schema_id(client, &self.spec.name, &self.value_schema),
        )?;
        Ok((key_id, value_id))
    }

    /// Fetch the built spec for a collection.
    async fn fetch_spec(
        client: &postgrest::Postgrest,
        collection: &str,
    ) -> anyhow::Result<Option<flow::CollectionSpec>> {
        #[derive(serde::Deserialize)]
        struct Row {
            built_spec: flow::CollectionSpec,
        }

        let mut rows: Vec<Row> = client
            .from("live_specs_ext")
            .eq("spec_type", "collection")
            .eq("catalog_name", collection)
            .select("built_spec")
            .execute()
            .await
            .and_then(|r| r.error_for_status())
            .context("listing current collection specifications")?
            .json()
            .await?;

        if let Some(Row { built_spec }) = rows.pop() {
            Ok(Some(built_spec))
        } else {
            Ok(None)
        }
    }

    /// Fetch the journals of a collection and map into stable-order partitions.
    async fn fetch_partitions(
        journal_client: &journal::Client,
        collection: &str,
    ) -> anyhow::Result<Vec<Partition>> {
        let request = broker::ListRequest {
            selector: Some(broker::LabelSelector {
                include: Some(labels::build_set([(labels::COLLECTION, collection)])),
                exclude: None,
            }),
            ..Default::default()
        };
        let response = journal_client.list(request).await?;
        let mut partitions = Vec::with_capacity(response.journals.len());

        for journal in response.journals {
            partitions.push(Partition {
                create_revision: journal.create_revision,
                spec: journal.spec.context("expected journal Spec")?,
                _mod_revision: journal.mod_revision,
                _route: journal.route.context("expected journal Route")?,
            })
        }

        // Establish stability of exposed partition indices by ordering journals
        // by their created revision, and _then_ by their name.
        partitions.sort_by(|l, r| {
            (l.create_revision, &l.spec.name).cmp(&(r.create_revision, &r.spec.name))
        });

        Ok(partitions)
    }

    /// Map a partition and timestamp into the newest covering fragment offset.
    pub async fn fetch_partition_offset(
        &self,
        partition_index: usize,
        timestamp_millis: i64,
    ) -> anyhow::Result<Option<(i64, i64)>> {
        let Some(partition) = self.partitions.get(partition_index) else {
            return Ok(None);
        };
        let (not_before_sec, _) = self.not_before.to_unix();

        let begin_mod_time = if timestamp_millis == -1 {
            i64::MAX // Sentinel for "largest available offset",
        } else if timestamp_millis == -2 {
            0 // Sentinel for "first available offset"
        } else {
            let timestamp = timestamp_millis / 1_000;
            if timestamp < not_before_sec as i64 {
                not_before_sec as i64
            } else {
                timestamp as i64
            }
        };

        let request = broker::FragmentsRequest {
            journal: partition.spec.name.clone(),
            begin_mod_time,
            page_limit: 1,
            ..Default::default()
        };
        let response = self.journal_client.list_fragments(request).await?;

        let (offset, mod_time) = match response.fragments.get(0) {
            Some(broker::fragments_response::Fragment {
                spec: Some(spec), ..
            }) => {
                if timestamp_millis == -1 {
                    // Subtract one to reflect the largest fetch-able offset of the fragment.
                    (spec.end - 1, spec.mod_time)
                } else {
                    (spec.begin, spec.mod_time)
                }
            }
            _ => (0, 0),
        };

        tracing::debug!(
            collection = self.spec.name,
            mod_time,
            offset,
            partition_index,
            timestamp_millis,
            "fetched offset"
        );

        Ok(Some((offset, mod_time)))
    }

    /// Build a journal client by resolving the collections data-plane gateway and an access token.
    async fn build_journal_client(
        client: &postgrest::Postgrest,
        collection: &str,
    ) -> anyhow::Result<journal::Client> {
        let body = serde_json::json!({
            "prefixes": [collection],
        })
        .to_string();

        #[derive(serde::Deserialize)]
        struct Auth {
            token: String,
            gateway_url: String,
        }

        let auth: [Auth; 1] = client
            .rpc("gateway_auth_token", body)
            .build()
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .context("requesting data plane gateway auth token")?
            .json()
            .await?;

        tracing::debug!(
            collection,
            gateway = auth[0].gateway_url,
            "fetched data-plane token"
        );

        let mut metadata = gazette::Metadata::default();
        metadata.bearer_token(&auth[0].token)?;

        let router = gazette::Router::new(&auth[0].gateway_url, "dekaf")?;
        let client = journal::Client::new(Default::default(), router, metadata);

        Ok(client)
    }

    async fn registered_schema_id(
        client: &postgrest::Postgrest,
        catalog_name: &str,
        schema: &avro::Schema,
    ) -> anyhow::Result<u32> {
        #[derive(serde::Deserialize)]
        struct Row {
            registry_id: u32,
        }

        // Note the canonical form of the schema strips away some important metadata
        // that we require while encoding, such as default values.
        // It's fully sufficient for readers, though.
        // We map into a serde_json::Value to ensure stability of property order when content-summing.
        let schema: serde_json::Value = serde_json::from_str(&schema.canonical_form()).unwrap();
        let schema_md5 = format!("{:x}", md5::compute(&schema.to_string()));

        let mut rows: Vec<Row> = client
            .from("registered_avro_schemas")
            .eq("avro_schema_md5", &schema_md5)
            .select("registry_id")
            .execute()
            .await
            .and_then(|r| r.error_for_status())
            .context("querying for an already-registered schema")?
            .json()
            .await?;

        if let Some(Row { registry_id }) = rows.pop() {
            return Ok(registry_id);
        }

        let mut rows: Vec<Row> = client
            .from("registered_avro_schemas")
            .insert(
                serde_json::json!([{
                    "avro_schema": schema,
                    "catalog_name": catalog_name,
                }])
                .to_string(),
            )
            .execute()
            .await
            .and_then(|r| r.error_for_status())
            .context("inserting new registered schema")?
            .json()
            .await?;

        let registry_id = rows.pop().unwrap().registry_id;
        tracing::info!(schema_md5, registry_id, "registered new Avro schema");

        Ok(registry_id)
    }
}
