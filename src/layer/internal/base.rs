//! Base layer implementation.
//!
//! A base layer stores triple data without referring to a parent.
use bytes::Bytes;
use futures::stream::{Peekable, Stream, StreamExt};
use futures::task::{Context, Poll};

use super::super::builder::*;
use super::super::id_map::*;
use super::super::layer::*;
use crate::layer::InternalLayer;
use crate::structure::*;
use crate::{chrono_log, storage::*};

use std::io;
use std::pin::Pin;

/// A base layer.
///
/// This layer type has no parent, and therefore does not store any
/// additions or removals. It stores all its triples plus indexes
/// directly.
#[derive(Clone)]
pub struct BaseLayer {
    pub(super) name: [u32; 5],
    pub(super) node_dictionary: StringDict,
    pub(super) predicate_dictionary: StringDict,
    pub(super) value_dictionary: TypedDict,

    pub(super) node_value_idmap: IdMap,
    pub(super) predicate_idmap: IdMap,

    pub(super) subjects: Option<MonotonicLogArray>,
    pub(super) objects: Option<MonotonicLogArray>,

    pub(super) s_p_adjacency_list: AdjacencyList,
    pub(super) sp_o_adjacency_list: AdjacencyList,
    pub(super) o_ps_adjacency_list: AdjacencyList,

    pub(super) predicate_wavelet_tree: WaveletTree,
}

impl BaseLayer {
    pub async fn load_from_files<F: FileLoad + FileStore>(
        name: [u32; 5],
        files: &BaseLayerFiles<F>,
    ) -> io::Result<InternalLayer> {
        let maps = files.map_all().await?;
        Ok(Self::load(name, maps))
    }

    pub fn load(name: [u32; 5], maps: BaseLayerMaps) -> InternalLayer {
        let node_dictionary = StringDict::parse(
            maps.node_dictionary_maps.offsets_map,
            maps.node_dictionary_maps.blocks_map,
        );
        let predicate_dictionary = StringDict::parse(
            maps.predicate_dictionary_maps.offsets_map,
            maps.predicate_dictionary_maps.blocks_map,
        );
        let value_dictionary = TypedDict::from_parts(
            maps.value_dictionary_maps.types_present_map,
            maps.value_dictionary_maps.type_offsets_map,
            maps.value_dictionary_maps.offsets_map,
            maps.value_dictionary_maps.blocks_map,
        );

        let node_value_idmap = match maps.id_map_maps.node_value_idmap_maps {
            None => IdMap::default(),
            Some(maps) => IdMap::from_maps(
                maps,
                util::calculate_width(
                    (node_dictionary.num_entries() + value_dictionary.num_entries()) as u64,
                ),
            ),
        };

        let predicate_idmap = match maps.id_map_maps.predicate_idmap_maps {
            None => IdMap::default(),
            Some(map) => IdMap::from_maps(
                map,
                util::calculate_width(predicate_dictionary.num_entries() as u64),
            ),
        };

        let subjects = maps.subjects_map.map(|subjects_map| {
            MonotonicLogArray::from_logarray(LogArray::parse(subjects_map).unwrap())
        });
        let objects = maps.objects_map.map(|objects_map| {
            MonotonicLogArray::from_logarray(LogArray::parse(objects_map).unwrap())
        });

        let s_p_adjacency_list = AdjacencyList::parse(
            maps.s_p_adjacency_list_maps.nums_map,
            maps.s_p_adjacency_list_maps.bitindex_maps.bits_map,
            maps.s_p_adjacency_list_maps.bitindex_maps.blocks_map,
            maps.s_p_adjacency_list_maps.bitindex_maps.sblocks_map,
        );
        let sp_o_adjacency_list = AdjacencyList::parse(
            maps.sp_o_adjacency_list_maps.nums_map,
            maps.sp_o_adjacency_list_maps.bitindex_maps.bits_map,
            maps.sp_o_adjacency_list_maps.bitindex_maps.blocks_map,
            maps.sp_o_adjacency_list_maps.bitindex_maps.sblocks_map,
        );
        let o_ps_adjacency_list = AdjacencyList::parse(
            maps.o_ps_adjacency_list_maps.nums_map,
            maps.o_ps_adjacency_list_maps.bitindex_maps.bits_map,
            maps.o_ps_adjacency_list_maps.bitindex_maps.blocks_map,
            maps.o_ps_adjacency_list_maps.bitindex_maps.sblocks_map,
        );

        let predicate_wavelet_tree_width = s_p_adjacency_list.nums().width();
        let predicate_wavelet_tree = WaveletTree::from_parts(
            BitIndex::from_maps(
                maps.predicate_wavelet_tree_maps.bits_map,
                maps.predicate_wavelet_tree_maps.blocks_map,
                maps.predicate_wavelet_tree_maps.sblocks_map,
            ),
            predicate_wavelet_tree_width,
        );

        InternalLayer::Base(BaseLayer {
            name,
            node_dictionary,
            predicate_dictionary,
            value_dictionary,

            node_value_idmap,
            predicate_idmap,

            subjects,
            objects,

            s_p_adjacency_list,
            sp_o_adjacency_list,

            o_ps_adjacency_list,

            predicate_wavelet_tree,
        })
    }
}

/// A builder for a base layer.
///
/// This builder takes node, predicate and value strings in lexical
/// order through the corresponding `add_<thing>` methods. When
/// they're all added, `into_phase2()` is to be called to turn this
/// builder into a second builder that takes triple data.
pub struct BaseLayerFileBuilder<F: 'static + FileLoad + FileStore> {
    files: BaseLayerFiles<F>,

    builder: DictionarySetFileBuilder<F>,
}

impl<F: 'static + FileLoad + FileStore + Clone> BaseLayerFileBuilder<F> {
    /// Create the builder from the given files.
    pub async fn from_files(files: &BaseLayerFiles<F>) -> io::Result<Self> {
        let builder = DictionarySetFileBuilder::from_files(
            files.node_dictionary_files.clone(),
            files.predicate_dictionary_files.clone(),
            files.value_dictionary_files.clone(),
            files.blank_counts_file.clone(),
        )
        .await?;

        Ok(BaseLayerFileBuilder {
            files: files.clone(),
            builder,
        })
    }

    /// Add a node string.
    ///
    /// Panics if the given node string is not a lexical successor of the previous node string.
    pub fn add_node(&mut self, node: &str) -> u64 {
        let id = self.builder.add_node(node);

        id
    }

    pub fn add_node_bytes(&mut self, node: Bytes) -> u64 {
        self.builder.add_node_bytes(node)
    }

    /// Add a predicate string.
    ///
    /// Panics if the given predicate string is not a lexical successor of the previous node string.
    pub fn add_predicate(&mut self, predicate: &str) -> u64 {
        let id = self.builder.add_predicate(predicate);

        id
    }

    pub fn add_predicate_bytes(&mut self, predicate: Bytes) -> u64 {
        self.builder.add_predicate_bytes(predicate)
    }

    /// Add a value string.
    ///
    /// Panics if the given value string is not a lexical successor of the previous value string.
    pub fn add_value(&mut self, value: TypedDictEntry) -> u64 {
        let id = self.builder.add_value(value);

        id
    }

    /// Add nodes from an iterable.
    ///
    /// Panics if the nodes are not in lexical order, or if previous added nodes are a lexical succesor of any of these nodes.
    pub fn add_nodes<I: 'static + IntoIterator<Item = String> + Send>(
        &mut self,
        nodes: I,
    ) -> Vec<u64>
    where
        <I as std::iter::IntoIterator>::IntoIter: Unpin + Send + Sync,
        I: Unpin + Sync,
    {
        let ids = self.builder.add_nodes(nodes);

        ids
    }

    pub fn add_nodes_bytes<I: 'static + IntoIterator<Item = Bytes> + Send>(
        &mut self,
        nodes: I,
    ) -> Vec<u64>
    where
        <I as std::iter::IntoIterator>::IntoIter: Unpin + Send + Sync,
        I: Unpin + Sync,
    {
        let ids = self.builder.add_nodes_bytes(nodes);

        ids
    }

    /// Add predicates from an iterable.
    ///
    /// Panics if the predicates are not in lexical order, or if previous added predicates are a lexical succesor of any of these predicates.
    pub fn add_predicates<I: 'static + IntoIterator<Item = String> + Send>(
        &mut self,
        predicates: I,
    ) -> Vec<u64>
    where
        <I as std::iter::IntoIterator>::IntoIter: Unpin + Send + Sync,
        I: Unpin + Sync,
    {
        let ids = self.builder.add_predicates(predicates);

        ids
    }

    pub fn add_predicates_bytes<I: 'static + IntoIterator<Item = Bytes> + Send>(
        &mut self,
        predicates: I,
    ) -> Vec<u64>
    where
        <I as std::iter::IntoIterator>::IntoIter: Unpin + Send + Sync,
        I: Unpin + Sync,
    {
        let ids = self.builder.add_predicates_bytes(predicates);

        ids
    }

    /// Add values from an iterable.
    ///
    /// Panics if the values are not in lexical order, or if previous added values are a lexical succesor of any of these values.
    pub fn add_values<I: 'static + IntoIterator<Item = TypedDictEntry> + Send>(
        &mut self,
        values: I,
    ) -> Vec<u64>
    where
        <I as std::iter::IntoIterator>::IntoIter: Unpin + Send + Sync,
        I: Unpin + Sync,
    {
        let ids = self.builder.add_values(values);

        ids
    }

    /// Turn this builder into a phase 2 builder that will take triple data.
    pub async fn into_phase2(self) -> io::Result<BaseLayerFileBuilderPhase2<F>> {
        let BaseLayerFileBuilder { files, builder } = self;

        let blank_node_count = builder.blank_node_count();
        builder.finalize().await?;

        let node_dict_blocks_map = files.node_dictionary_files.blocks_file.map().await?;
        let node_dict_offsets_map = files.node_dictionary_files.offsets_file.map().await?;
        let predicate_dict_blocks_map = files.predicate_dictionary_files.blocks_file.map().await?;
        let predicate_dict_offsets_map =
            files.predicate_dictionary_files.offsets_file.map().await?;
        let value_dict_types_present_map = files
            .value_dictionary_files
            .types_present_file
            .map()
            .await?;
        let value_dict_type_offsets_map =
            files.value_dictionary_files.type_offsets_file.map().await?;
        let value_dict_blocks_map = files.value_dictionary_files.blocks_file.map().await?;
        let value_dict_offsets_map = files.value_dictionary_files.offsets_file.map().await?;

        let node_dict = StringDict::parse(node_dict_offsets_map, node_dict_blocks_map);
        let pred_dict = StringDict::parse(predicate_dict_offsets_map, predicate_dict_blocks_map);
        let val_dict = TypedDict::from_parts(
            value_dict_types_present_map,
            value_dict_type_offsets_map,
            value_dict_offsets_map,
            value_dict_blocks_map,
        );

        // TODO: it is a bit silly to parse the dictionaries just for this. surely we can get the counts in an easier way?
        let num_nodes = node_dict.num_entries() + blank_node_count as usize;
        let num_predicates = pred_dict.num_entries();
        let num_values = val_dict.num_entries();

        BaseLayerFileBuilderPhase2::new(files, num_nodes, num_predicates, num_values).await
    }
}

/// Second phase of base layer building.
///
/// This builder takes ordered triple data. When all data has been
/// added, `finalize()` will build a layer.
pub struct BaseLayerFileBuilderPhase2<F: 'static + FileLoad + FileStore> {
    files: BaseLayerFiles<F>,

    builder: TripleFileBuilder<F>,
}

impl<F: 'static + FileLoad + FileStore> BaseLayerFileBuilderPhase2<F> {
    pub async fn new(
        files: BaseLayerFiles<F>,

        num_nodes: usize,
        num_predicates: usize,
        num_values: usize,
    ) -> io::Result<Self> {
        let builder = TripleFileBuilder::new(
            files.s_p_adjacency_list_files.clone(),
            files.sp_o_adjacency_list_files.clone(),
            num_nodes,
            num_predicates,
            num_values,
            None,
        )
        .await?;

        Ok(BaseLayerFileBuilderPhase2 { files, builder })
    }

    /// Add the given subject, predicate and object.
    ///
    /// This will panic if a greater triple has already been added.
    pub async fn add_triple(
        &mut self,
        subject: u64,
        predicate: u64,
        object: u64,
    ) -> io::Result<()> {
        self.builder.add_triple(subject, predicate, object).await
    }

    /// Add the given triple.
    ///
    /// This will panic if a greater triple has already been added.
    pub async fn add_id_triples<I: 'static + IntoIterator<Item = IdTriple>>(
        &mut self,
        triples: I,
    ) -> io::Result<()>
    where
        <I as std::iter::IntoIterator>::IntoIter: Unpin + Send,
    {
        self.builder.add_id_triples(triples).await
    }

    pub(crate) async fn partial_finalize(self) -> io::Result<BaseLayerFiles<F>> {
        self.builder.finalize().await?;
        chrono_log!("finalized base triples builder");

        Ok(self.files)
    }

    pub async fn finalize(self) -> io::Result<()> {
        self.builder.finalize().await?;
        chrono_log!("finalized base triples builder");
        let s_p_adjacency_list_files = self.files.s_p_adjacency_list_files.clone();
        let sp_o_adjacency_list_files = self.files.sp_o_adjacency_list_files.clone();
        let o_ps_adjacency_list_files = self.files.o_ps_adjacency_list_files.clone();
        let predicate_wavelet_tree_files = self.files.predicate_wavelet_tree_files.clone();
        build_indexes(
            s_p_adjacency_list_files,
            sp_o_adjacency_list_files,
            o_ps_adjacency_list_files,
            None,
            predicate_wavelet_tree_files,
        )
        .await?;

        chrono_log!("finalized base builder");

        Ok(())
    }
}

pub struct BaseTripleStream<S: Stream<Item = io::Result<(u64, u64)>> + Send> {
    s_p_stream: Peekable<S>,
    sp_o_stream: Peekable<S>,
    last_s_p: (u64, u64),
    last_sp: u64,
}

impl<S: Stream<Item = io::Result<(u64, u64)>> + Unpin + Send> BaseTripleStream<S> {
    pub fn new(s_p_stream: S, sp_o_stream: S) -> BaseTripleStream<S> {
        BaseTripleStream {
            s_p_stream: s_p_stream.peekable(),
            sp_o_stream: sp_o_stream.peekable(),
            last_s_p: (0, 0),
            last_sp: 0,
        }
    }
}

impl<S: Stream<Item = io::Result<(u64, u64)>> + Unpin + Send> Stream for BaseTripleStream<S> {
    type Item = io::Result<(u64, u64, u64)>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<io::Result<(u64, u64, u64)>>> {
        let peeked = Pin::new(&mut self.sp_o_stream).poll_peek(cx);
        match peeked {
            Poll::Ready(Some(Ok((sp, o)))) => {
                let sp = *sp;
                let o = *o;
                if sp > self.last_sp {
                    let peeked = Pin::new(&mut self.s_p_stream).poll_peek(cx);
                    match peeked {
                        Poll::Ready(None) => Poll::Ready(Some(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "unexpected end of s_p_stream",
                        )))),
                        Poll::Ready(Some(Ok((s, p)))) => {
                            let s = *s;
                            let p = *p;
                            util::assert_poll_next(Pin::new(&mut self.s_p_stream), cx).unwrap();
                            util::assert_poll_next(Pin::new(&mut self.sp_o_stream), cx).unwrap();

                            self.last_s_p = (s, p);
                            self.last_sp = sp;

                            Poll::Ready(Some(Ok((s, p, o))))
                        }
                        Poll::Ready(Some(Err(_))) => Poll::Ready(Some(Err(
                            util::assert_poll_next(Pin::new(&mut self.s_p_stream), cx)
                                .err()
                                .unwrap(),
                        ))),
                        Poll::Pending => Poll::Pending,
                    }
                } else {
                    util::assert_poll_next(Pin::new(&mut self.sp_o_stream), cx).unwrap();

                    Poll::Ready(Some(Ok((self.last_s_p.0, self.last_s_p.1, o))))
                }
            }
            Poll::Ready(Some(Err(_))) => Poll::Ready(Some(Err(util::assert_poll_next(
                Pin::new(&mut self.sp_o_stream),
                cx,
            )
            .err()
            .unwrap()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub async fn open_base_triple_stream<F: 'static + FileLoad + FileStore>(
    s_p_files: AdjacencyListFiles<F>,
    sp_o_files: AdjacencyListFiles<F>,
) -> io::Result<impl Stream<Item = io::Result<(u64, u64, u64)>> + Unpin + Send> {
    let s_p_stream =
        adjacency_list_stream_pairs(s_p_files.bitindex_files.bits_file, s_p_files.nums_file)
            .await?;
    let sp_o_stream =
        adjacency_list_stream_pairs(sp_o_files.bitindex_files.bits_file, sp_o_files.nums_file)
            .await?;

    Ok(BaseTripleStream::new(s_p_stream, sp_o_stream))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::storage::memory::*;
    use futures::stream::TryStreamExt;

    pub fn base_layer_files() -> BaseLayerFiles<MemoryBackedStore> {
        // TODO inline
        base_layer_memory_files()
    }

    pub async fn example_base_layer_files() -> io::Result<BaseLayerFiles<MemoryBackedStore>> {
        let nodes = vec!["aaaaa", "baa", "bbbbb", "ccccc", "mooo"];
        let predicates = vec!["abcde", "fghij", "klmno", "lll"];
        let values = vec!["chicken", "cow", "dog", "pig", "zebra"];

        let base_layer_files = base_layer_files();

        let mut builder = BaseLayerFileBuilder::from_files(&base_layer_files).await?;

        builder.add_nodes(nodes.into_iter().map(|s| s.to_string()));
        builder.add_predicates(predicates.into_iter().map(|s| s.to_string()));
        builder.add_values(values.into_iter().map(|s| String::make_entry(&s)));

        let mut builder = builder.into_phase2().await?;

        builder.add_triple(1, 1, 1).await?;
        builder.add_triple(2, 1, 1).await?;
        builder.add_triple(2, 1, 3).await?;
        builder.add_triple(2, 3, 6).await?;
        builder.add_triple(3, 2, 5).await?;
        builder.add_triple(3, 3, 6).await?;
        builder.add_triple(4, 3, 6).await?;

        builder.finalize().await?;

        Ok(base_layer_files)
    }

    pub async fn example_base_layer() -> InternalLayer {
        let base_layer_files = example_base_layer_files().await.unwrap();

        BaseLayer::load_from_files([1, 2, 3, 4, 5], &base_layer_files)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn build_and_query_base_layer() {
        let layer = example_base_layer().await;

        assert!(layer.triple_exists(1, 1, 1));
        assert!(layer.triple_exists(2, 1, 1));
        assert!(layer.triple_exists(2, 1, 3));
        assert!(layer.triple_exists(2, 3, 6));
        assert!(layer.triple_exists(3, 2, 5));
        assert!(layer.triple_exists(3, 3, 6));
        assert!(layer.triple_exists(4, 3, 6));

        assert!(!layer.triple_exists(2, 2, 0));
    }

    #[tokio::test]
    async fn dictionary_entries_in_base() {
        let base_layer = example_base_layer().await;

        assert_eq!(3, base_layer.subject_id("bbbbb".into()).unwrap());
        assert_eq!(2, base_layer.predicate_id("fghij".into()).unwrap());
        assert_eq!(1, base_layer.object_node_id("aaaaa".into()).unwrap());
        assert_eq!(
            6,
            base_layer
                .object_value_id(&String::make_entry(&"chicken"))
                .unwrap()
        );

        assert_eq!(Blankable::Val("bbbbb"), base_layer.id_subject(3).unwrap());
        assert_eq!(Blankable::Val("fghij"), base_layer.id_predicate(2).unwrap());
        assert_eq!(
            ObjectType::new_string_node("aaaaa".to_string()),
            base_layer.id_object(1).unwrap()
        );
        assert_eq!(
            ObjectType::Value(String::make_entry(&"chicken")),
            base_layer.id_object(6).unwrap()
        );
    }

    #[tokio::test]
    async fn everything_iterator() {
        let layer = example_base_layer().await;
        let triples: Vec<_> = layer
            .triples()
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();

        assert_eq!(
            vec![
                (1, 1, 1),
                (2, 1, 1),
                (2, 1, 3),
                (2, 3, 6),
                (3, 2, 5),
                (3, 3, 6),
                (4, 3, 6)
            ],
            triples
        );
    }

    #[tokio::test]
    async fn lookup_by_object() {
        let layer = example_base_layer().await;

        let triples: Vec<_> = layer
            .triples_o(1)
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();
        assert_eq!(vec![(1, 1, 1), (2, 1, 1)], triples);

        let triples: Vec<_> = layer
            .triples_o(3)
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();
        assert_eq!(vec![(2, 1, 3)], triples);

        let triples: Vec<_> = layer
            .triples_o(5)
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();
        assert_eq!(vec![(3, 2, 5)], triples);

        let triples: Vec<_> = layer
            .triples_o(6)
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();
        assert_eq!(vec![(2, 3, 6), (3, 3, 6), (4, 3, 6)], triples);
    }

    #[tokio::test]
    async fn lookup_by_predicate() {
        let layer = example_base_layer().await;

        let pairs: Vec<_> = layer
            .triples_p(1)
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();

        assert_eq!(vec![(1, 1, 1), (2, 1, 1), (2, 1, 3)], pairs);

        let pairs: Vec<_> = layer
            .triples_p(2)
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();

        assert_eq!(vec![(3, 2, 5)], pairs);

        let pairs: Vec<_> = layer
            .triples_p(3)
            .map(|t| (t.subject, t.predicate, t.object))
            .collect();

        assert_eq!(vec![(2, 3, 6), (3, 3, 6), (4, 3, 6)], pairs);

        assert!(layer.triples_p(4).next().is_none());
    }

    #[tokio::test]
    async fn create_empty_base_layer() {
        let base_layer_files = base_layer_files();
        let builder = BaseLayerFileBuilder::from_files(&base_layer_files)
            .await
            .unwrap();

        let builder = builder.into_phase2().await.unwrap();
        builder.finalize().await.unwrap();

        let layer = BaseLayer::load_from_files([1, 2, 3, 4, 5], &base_layer_files)
            .await
            .unwrap();

        assert_eq!(0, layer.node_and_value_count());
        assert_eq!(0, layer.predicate_count());
    }

    #[tokio::test]
    async fn stream_base_triples() {
        let layer_files = example_base_layer_files().await.unwrap();

        let stream = open_base_triple_stream(
            layer_files.s_p_adjacency_list_files,
            layer_files.sp_o_adjacency_list_files,
        )
        .await
        .unwrap();

        let triples: Vec<_> = stream.try_collect().await.unwrap();

        assert_eq!(
            vec![
                (1, 1, 1),
                (2, 1, 1),
                (2, 1, 3),
                (2, 3, 6),
                (3, 2, 5),
                (3, 3, 6),
                (4, 3, 6)
            ],
            triples
        );
    }

    #[tokio::test]
    async fn count_triples() {
        let layer = example_base_layer().await;

        assert_eq!(7, layer.internal_triple_layer_addition_count());
        assert_eq!(0, layer.internal_triple_layer_removal_count());
        assert_eq!(7, layer.triple_addition_count());
        assert_eq!(0, layer.triple_removal_count());
        assert_eq!(7, layer.triple_count());
    }

    #[tokio::test]
    async fn count_triples_of_empty_base_layer() {
        let layer_files = base_layer_files();
        let builder = BaseLayerFileBuilder::from_files(&layer_files)
            .await
            .unwrap();
        builder
            .into_phase2()
            .await
            .unwrap()
            .finalize()
            .await
            .unwrap();
        let layer = BaseLayer::load_from_files([1, 2, 3, 4, 5], &layer_files)
            .await
            .unwrap();

        assert_eq!(0, layer.internal_triple_layer_addition_count());
        assert_eq!(0, layer.internal_triple_layer_removal_count());
        assert_eq!(0, layer.triple_count());
        assert_eq!(0, layer.triple_addition_count());
        assert_eq!(0, layer.triple_removal_count());
    }
}
