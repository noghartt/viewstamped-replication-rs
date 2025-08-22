use std::fmt::Debug;

pub trait StateMachine: Debug + 'static {
    type Input: Clone;
    type Output: Clone;

    fn apply(&mut self, input: Self::Input) -> Self::Output;
}
