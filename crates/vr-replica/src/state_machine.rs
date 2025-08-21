use std::fmt::Debug;

pub trait StateMachine: Debug + Clone + 'static {
    type State: Debug + Clone;
    type Input: Debug + Clone;
    type Output: Debug + Clone;

    fn apply(&mut self, input: Self::Input) -> Self::Output;
}
