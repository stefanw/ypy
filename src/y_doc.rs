use std::cell::RefCell;
use std::rc::Rc;
use std::rc::Weak;

use crate::y_array::YArray;
use crate::y_map::YMap;
use crate::y_text::YText;
use crate::y_transaction::YTransaction;
use crate::y_transaction::YTransactionWrapper;
use crate::y_xml::YXmlElement;
use crate::y_xml::YXmlText;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3::types::PyTuple;
use yrs::updates::encoder::Encode;
use yrs::Doc;
use yrs::OffsetKind;
use yrs::Options;
use yrs::SubscriptionId;
use yrs::Transact;
use yrs::TransactionCleanupEvent;
use yrs::TransactionMut;

pub trait WithDoc<T> {
    fn with_doc(self, doc: Rc<RefCell<YDocInner>>) -> T;
}
pub trait WithTransaction {
    fn get_doc(&self) -> Rc<RefCell<YDocInner>>;

    fn with_transaction<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&YTransaction) -> R,
    {
        let txn = self.get_transaction();
        let mut txn = txn.borrow_mut();
        let result = f(&mut txn);
        result
    }

    fn get_transaction(&self) -> Rc<RefCell<YTransaction>> {
        let doc = self.get_doc();
        let txn = doc.borrow_mut().begin_transaction();
        txn
    }
}

pub struct YDocInner {
    doc: Doc,
    txn: Option<Weak<RefCell<YTransaction>>>,
}

impl YDocInner {
    pub fn begin_transaction(&mut self) -> Rc<RefCell<YTransaction>> {
        // Check if we think we still have a transaction
        if let Some(weak_txn) = &self.txn {
            // And if it's actually around
            if let Some(txn) = weak_txn.upgrade() {
                if !txn.borrow().committed {
                    return txn;
                }
            }
        }
        // HACK: get rid of lifetime
        let txn = unsafe {
            std::mem::transmute::<TransactionMut, TransactionMut<'static>>(self.doc.transact_mut())
        };
        let txn = YTransaction::new(txn);
        let txn = Rc::new(RefCell::new(txn));
        self.txn = Some(Rc::downgrade(&txn));
        txn
    }

    pub fn transact_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut YTransaction) -> R,
    {
        // HACK: get rid of lifetime
        let txn = unsafe {
            std::mem::transmute::<TransactionMut, TransactionMut<'static>>(self.doc.transact_mut())
        };
        let mut txn = YTransaction::new(txn);
        f(&mut txn)
    }
}

/// A Ypy document type. Documents are most important units of collaborative resources management.
/// All shared collections live within a scope of their corresponding documents. All updates are
/// generated on per document basis (rather than individual shared type). All operations on shared
/// collections happen via [YTransaction], which lifetime is also bound to a document.
///
/// Document manages so called root types, which are top-level shared types definitions (as opposed
/// to recursively nested types).
///
/// A basic workflow sample:
///
/// ```python
/// from y_py import YDoc
///
/// doc = YDoc()
/// with doc.begin_transaction() as txn:
///     text = txn.get_text('name')
///     text.push(txn, 'hello world')
///     output = text.to_string(txn)
///     print(output)
/// ```
#[pyclass(unsendable, subclass)]
pub struct YDoc {
    pub inner: Rc<RefCell<YDocInner>>,
}

#[pymethods]
impl YDoc {
    /// Creates a new Ypy document. If `client_id` parameter was passed it will be used as this
    /// document globally unique identifier (it's up to caller to ensure that requirement).
    /// Otherwise it will be assigned a randomly generated number.
    #[new]
    pub fn new(
        client_id: Option<u64>,
        offset_kind: Option<String>,
        skip_gc: Option<bool>,
    ) -> PyResult<Self> {
        let mut options = Options::default();
        if let Some(client_id) = client_id {
            options.client_id = client_id;
        }

        if let Some(raw_offset) = offset_kind {
            let clean_offset = raw_offset.to_lowercase().replace("-", "");
            let offset = match clean_offset.as_str() {
                "utf8" => Ok(OffsetKind::Bytes),
                "utf16" => Ok(OffsetKind::Utf16),
                "utf32" => Ok(OffsetKind::Utf32),
                _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "'{}' is not a valid offset kind (utf8, utf16, or utf32).",
                    clean_offset
                ))),
            }?;
            options.offset_kind = offset;
        }

        if let Some(skip_gc) = skip_gc {
            options.skip_gc = skip_gc;
        }

        let inner = YDocInner {
            doc: Doc::with_options(options),
            txn: None,
        };

        Ok(YDoc {
            inner: Rc::new(RefCell::new(inner)),
        })
    }

    /// Gets globally unique identifier of this `YDoc` instance.
    #[getter]
    pub fn client_id(&self) -> u64 {
        self.inner.borrow().doc.client_id()
    }

    /// Returns a new transaction for this document. Ypy shared data types execute their
    /// operations in a context of a given transaction. Each document can have only one active
    /// transaction at the time - subsequent attempts will cause exception to be thrown.
    ///
    /// Transactions started with `doc.begin_transaction` can be released by deleting the transaction object
    /// method.
    ///
    /// Example:
    ///
    /// ```python
    /// from y_py import YDoc
    /// doc = YDoc()
    /// text = doc.get_text('name')
    /// with doc.begin_transaction() as txn:
    ///     text.insert(txn, 0, 'hello world')
    /// ```
    pub fn begin_transaction(&self) -> YTransactionWrapper {
        YTransactionWrapper::new(self.inner.borrow_mut().begin_transaction())
    }

    pub fn transact(&mut self, callback: PyObject) -> PyResult<PyObject> {
        let txn = YTransactionWrapper::new(self.inner.borrow_mut().begin_transaction());
        let result = Python::with_gil(|py| {
            let args = PyTuple::new(py, vec![txn.into_py(py)]);
            let result = callback.call(py, args, None);
            result
        });
        self.inner.borrow_mut().txn = None;
        result
    }

    /// Returns a `YMap` shared data type, that's accessible for subsequent accesses using given
    /// `name`.
    ///
    /// If there was no instance with this name before, it will be created and then returned.
    ///
    /// If there was an instance with this name, but it was of different type, it will be projected
    /// onto `YMap` instance.
    pub fn get_map(&mut self, name: &str) -> YMap {
        self.inner.borrow().doc.get_or_insert_map(name).with_doc(self.inner.clone())
    }

    /// Returns a `YXmlElement` shared data type, that's accessible for subsequent accesses using
    /// given `name`.
    ///
    /// If there was no instance with this name before, it will be created and then returned.
    ///
    /// If there was an instance with this name, but it was of different type, it will be projected
    /// onto `YXmlElement` instance.
    pub fn get_xml_element(&mut self, name: &str) -> YXmlElement {
        self.inner
            .borrow()
            .doc
            .get_or_insert_xml_element(name)
            .with_doc(self.inner.clone())
    }

    /// Returns a `YXmlText` shared data type, that's accessible for subsequent accesses using given
    /// `name`.
    ///
    /// If there was no instance with this name before, it will be created and then returned.
    ///
    /// If there was an instance with this name, but it was of different type, it will be projected
    /// onto `YXmlText` instance.
    pub fn get_xml_text(&mut self, name: &str) -> YXmlText {
        self.inner.borrow().doc.get_or_insert_xml_text(name).with_doc(self.inner.clone())
    }

    /// Returns a `YArray` shared data type, that's accessible for subsequent accesses using given
    /// `name`.
    ///
    /// If there was no instance with this name before, it will be created and then returned.
    ///
    /// If there was an instance with this name, but it was of different type, it will be projected
    /// onto `YArray` instance.
    pub fn get_array(&mut self, name: &str) -> YArray {
        self.inner
            .borrow()
            .doc
            .get_or_insert_array(&name)
            .with_doc(self.inner.clone())
    }

    /// Returns a `YText` shared data type, that's accessible for subsequent accesses using given
    /// `name`.
    ///
    /// If there was no instance with this name before, it will be created and then returned.
    ///
    /// If there was an instance with this name, but it was of different type, it will be projected
    /// onto `YText` instance.
    pub fn get_text(&mut self, name: &str) -> YText {
        self.inner
            .borrow()
            .doc
            .get_or_insert_text(&name)
            .with_doc(self.inner.clone())
    }

    /// Subscribes a callback to a `YDoc` lifecycle event.
    pub fn observe_after_transaction(&mut self, callback: PyObject) -> SubscriptionId {
        self.inner
            .borrow()
            .doc
            .observe_transaction_cleanup(move |txn, event| {
                Python::with_gil(|py| {
                    let event = AfterTransactionEvent::new(event, txn);
                    if let Err(err) = callback.call1(py, (event,)) {
                        err.restore(py)
                    }
                })
            })
            .unwrap()
            .into()
    }
}

/// Encodes a state vector of a given Ypy document into its binary representation using lib0 v1
/// encoding. State vector is a compact representation of updates performed on a given document and
/// can be used by `encode_state_as_update` on remote peer to generate a delta update payload to
/// synchronize changes between peers.
///
/// Example:
///
/// ```python
/// from y_py import YDoc, encode_state_vector, encode_state_as_update, apply_update from y_py
///
/// # document on machine A
/// local_doc = YDoc()
/// local_sv = encode_state_vector(local_doc)
///
/// # document on machine B
/// remote_doc = YDoc()
/// remote_delta = encode_state_as_update(remote_doc, local_sv)
///
/// apply_update(local_doc, remote_delta)
/// ```
#[pyfunction]
pub fn encode_state_vector(doc: &mut YDoc) -> PyObject {
    let txn = doc.inner
        .borrow_mut()
        .begin_transaction();
    let txn = YTransactionWrapper::new(txn);
    txn.state_vector_v1()
}

/// Encodes all updates that have happened since a given version `vector` into a compact delta
/// representation using lib0 v1 encoding. If `vector` parameter has not been provided, generated
/// delta payload will contain all changes of a current Ypy document, working effectively as its
/// state snapshot.
///
/// Example:
///
/// ```python
/// from y_py import YDoc, encode_state_vector, encode_state_as_update, apply_update
///
/// # document on machine A
/// local_doc = YDoc()
/// local_sv = encode_state_vector(local_doc)
///
/// # document on machine B
/// remote_doc = YDoc()
/// remote_delta = encode_state_as_update(remote_doc, local_sv)
///
/// apply_update(local_doc, remote_delta)
/// ```
#[pyfunction]
pub fn encode_state_as_update(doc: &mut YDoc, vector: Option<Vec<u8>>) -> PyResult<PyObject> {
    let txn = doc.inner
        .borrow_mut()
        .begin_transaction();
    YTransactionWrapper::new(txn).diff_v1(vector)
}

/// Applies delta update generated by the remote document replica to a current document. This
/// method assumes that a payload maintains lib0 v1 encoding format.
///
/// Example:
///
/// ```python
/// from y_py import YDoc, encode_state_vector, encode_state_as_update, apply_update
///
/// # document on machine A
/// local_doc = YDoc()
/// local_sv = encode_state_vector(local_doc)
///
/// # document on machine B
/// remote_doc = YDoc()
/// remote_delta = encode_state_as_update(remote_doc, local_sv)
///
/// apply_update(local_doc, remote_delta)
/// ```
#[pyfunction]
pub fn apply_update(doc: &mut YDoc, diff: Vec<u8>) -> PyResult<()> {
    let txn = doc.inner
    .borrow_mut()
    .begin_transaction();
    YTransactionWrapper::new(txn).apply_v1(diff)?;

    Ok(())
}

#[pyclass(unsendable)]
pub struct AfterTransactionEvent {
    before_state: PyObject,
    after_state: PyObject,
    delete_set: PyObject,
    update: PyObject,
}

impl AfterTransactionEvent {
    fn new(event: &TransactionCleanupEvent, txn: &TransactionMut) -> Self {
        // Convert all event data into Python objects eagerly, so that we don't have to hold
        // on to the transaction.
        let before_state = event.before_state.encode_v1();
        let before_state: PyObject = Python::with_gil(|py| PyBytes::new(py, &before_state).into());
        let after_state = event.after_state.encode_v1();
        let after_state: PyObject = Python::with_gil(|py| PyBytes::new(py, &after_state).into());
        let delete_set = event.delete_set.encode_v1();
        let delete_set: PyObject = Python::with_gil(|py| PyBytes::new(py, &delete_set).into());
        let update = txn.encode_update_v1();
        let update = Python::with_gil(|py| PyBytes::new(py, &update).into());
        AfterTransactionEvent {
            before_state,
            after_state,
            delete_set,
            update,
        }
    }
}

#[pymethods]
impl AfterTransactionEvent {
    /// Returns a current shared type instance, that current event changes refer to.
    #[getter]
    pub fn before_state(&mut self) -> PyObject {
        self.before_state.clone()
    }

    #[getter]
    pub fn after_state(&mut self) -> PyObject {
        self.after_state.clone()
    }

    #[getter]
    pub fn delete_set(&mut self) -> PyObject {
        self.delete_set.clone()
    }

    pub fn get_update(&self) -> PyObject {
        self.update.clone()
    }
}
